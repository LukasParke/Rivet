package handlers

import (
	"context"
	"database/sql"
	"errors"
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
	dest, err := importer.ImportFile(*a.VerifiedPath, paths.LibraryRoots[0])
	if err != nil {
		_ = storage.MarkAcquisitionFailed(ctx, deps.DB, a.ID, err.Error())
		if deps.Hub != nil {
			deps.Hub.Publish("import.failed", map[string]any{"acquisitionId": a.ID, "error": err.Error()})
		}
		return err
	}
	_ = storage.MarkAcquisitionImported(ctx, deps.DB, a.ID, dest)
	if deps.Hub != nil {
		deps.Hub.Publish("import.completed", map[string]any{"acquisitionId": a.ID, "importedPath": dest})
	}
	return nil
}
