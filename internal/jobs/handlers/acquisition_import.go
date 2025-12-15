package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"path/filepath"
	"strings"
	"time"

	importer "github.com/LukeHagar/Rivet/internal/import"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type ImportDeps struct {
	DB  *sql.DB
	Hub publisher
}

func ImportAcquisition(ctx context.Context, deps ImportDeps, acquisitionID string) error {
	ctx, cancel := context.WithTimeout(ctx, 60*time.Second)
	defer cancel()

	a, err := storage.GetAcquisition(ctx, deps.DB, acquisitionID)
	if err != nil {
		return err
	}
	if a.VerifiedPath == nil || *a.VerifiedPath == "" {
		return errors.New("missing_verified_path")
	}
	paths, err := loadSetupPaths(ctx, deps.DB)
	if err != nil {
		return err
	}
	if len(paths.LibraryRoots) == 0 {
		return errors.New("no_library_roots")
	}

	if deps.Hub != nil {
		deps.Hub.Publish("import.started", map[string]any{"acquisitionId": a.ID, "verifiedPath": *a.VerifiedPath})
	}

	targetKind, targetName := loadTargetForImport(ctx, deps.DB, a.TargetID)
	destDir := computeLibraryDestDir(paths.LibraryRoots[0], targetKind, targetName)
	dest, err := importer.ImportFileToDir(*a.VerifiedPath, destDir)
	if err != nil {
		_ = storage.MarkAcquisitionFailed(ctx, deps.DB, a.ID, err.Error())
		if deps.Hub != nil {
			deps.Hub.Publish("import.failed", map[string]any{"acquisitionId": a.ID, "error": err.Error()})
		}
		return err
	}
	_ = storage.MarkAcquisitionImported(ctx, deps.DB, a.ID, dest)
	if deps.Hub != nil {
		deps.Hub.Publish("import.completed", map[string]any{"acquisitionId": a.ID, "importedPath": dest, "destDir": destDir})
	}
	return nil
}

func loadTargetForImport(ctx context.Context, db *sql.DB, targetID string) (kind string, name string) {
	targetID = strings.TrimSpace(targetID)
	if targetID == "" {
		return "", ""
	}
	row := db.QueryRowContext(ctx, `SELECT kind, external_source, external_id, payload_json FROM wanted_targets WHERE id = ?`, targetID)
	var k, src, ext string
	var payload sql.NullString
	if err := row.Scan(&k, &src, &ext, &payload); err != nil {
		return "", ""
	}
	kind = strings.TrimSpace(k)
	if payload.Valid {
		var m map[string]any
		if json.Unmarshal([]byte(payload.String), &m) == nil {
			if q, ok := m["query"].(string); ok {
				name = strings.TrimSpace(q)
			}
		}
	}
	if name == "" {
		if src != "" && ext != "" {
			name = strings.TrimSpace(src) + "-" + strings.TrimSpace(ext)
		} else {
			name = targetID
		}
	}
	return kind, name
}

func computeLibraryDestDir(libraryRoot, targetKind, targetName string) string {
	libraryRoot = filepath.Clean(strings.TrimSpace(libraryRoot))
	targetKind = strings.ToLower(strings.TrimSpace(targetKind))
	targetName = importer.SanitizePathComponent(targetName)

	switch targetKind {
	case "movie":
		return filepath.Join(libraryRoot, "Movies", targetName)
	case "episode", "season_pack", "tv":
		return filepath.Join(libraryRoot, "TV", targetName)
	default:
		return filepath.Join(libraryRoot, "imports", targetName)
	}
}
