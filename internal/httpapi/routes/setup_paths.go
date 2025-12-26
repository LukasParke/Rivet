package routes

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/storage"
	"github.com/LukeHagar/Rivet/internal/ws"
)

type SetupPathsRequest struct {
	DownloadsCompletedPath string   `json:"downloadsCompletedPath"`
	LibraryRoots           []string `json:"libraryRoots"`
	QuarantinePath         string   `json:"quarantinePath"`
}

type PathCheck struct {
	Path           string   `json:"path"`
	Exists         bool     `json:"exists"`
	IsDir          bool     `json:"isDir"`
	Readable       bool     `json:"readable"`
	Writable       bool     `json:"writable"`
	Errors         []string `json:"errors,omitempty"`
	Warnings       []string `json:"warnings,omitempty"`
	FilesystemID   string   `json:"filesystemId,omitempty"`
	HardlinkViable bool     `json:"hardlinkViable"`
}

type SetupPathsValidateResponse struct {
	Ok bool `json:"ok"`

	DownloadsCompleted PathCheck   `json:"downloadsCompleted"`
	LibraryRoots       []PathCheck `json:"libraryRoots"`
	Quarantine         PathCheck   `json:"quarantine"`
}

type SetupPathsSaveResponse struct {
	Saved bool `json:"saved"`
}

type SetupPathsDeps struct {
	DB   *sql.DB
	Hub  *ws.Hub
	DryRun bool
}

func RegisterSetupPaths(api fiber.Router, deps SetupPathsDeps) {
	grp := api.Group("/setup")

	grp.Post("/paths/validate", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		var req SetupPathsRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 8*time.Second)
		defer cancel()

		resp, err := validatePaths(ctx, deps, req, deps.DryRun)
		if err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_paths", "message": err.Error()})
		}
		return c.JSON(resp)
	})

	grp.Post("/paths/save", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		var req SetupPathsRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 8*time.Second)
		defer cancel()

		resp, err := validatePaths(ctx, deps, req, deps.DryRun)
		if err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_paths", "message": err.Error()})
		}
		// In dry run mode, allow saving even if paths don't exist (they'll be validated when actually used)
		if !resp.Ok && !deps.DryRun {
			return c.Status(http.StatusBadRequest).JSON(resp)
		}

		b, _ := json.Marshal(req)
		if err := storage.UpsertSetting(ctx, deps.DB, "setup.paths", string(b)); err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "save_failed"})
		}

		var actor *string
		if u, ok := middleware.UserFromContext(c); ok {
			actor = &u.ID
		}
		_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{Action: "setup.paths_saved", ActorUserID: actor})
		emitAudit(deps.Hub, "setup.paths_saved", actor, nil)
		return c.JSON(SetupPathsSaveResponse{Saved: true})
	})
}

func validatePaths(ctx context.Context, deps SetupPathsDeps, req SetupPathsRequest, dryRun bool) (SetupPathsValidateResponse, error) {
	if deps.Hub != nil {
		deps.Hub.Publish("setup.paths.validation.started", map[string]any{
			"dryRun":            dryRun,
			"libraryRootsCount": len(req.LibraryRoots),
		})
	}

	dl := cleanPath(req.DownloadsCompletedPath)
	q := cleanPath(req.QuarantinePath)
	if dl == "" {
		if deps.Hub != nil {
			deps.Hub.Publish("setup.paths.validation.error", map[string]any{
				"error":   "missing_downloads_completed_path",
				"message": "downloadsCompletedPath is required",
			})
		}
		return SetupPathsValidateResponse{}, errors.New("downloadsCompletedPath is required")
	}
	if q == "" {
		if deps.Hub != nil {
			deps.Hub.Publish("setup.paths.validation.error", map[string]any{
				"error":   "missing_quarantine_path",
				"message": "quarantinePath is required",
			})
		}
		return SetupPathsValidateResponse{}, errors.New("quarantinePath is required")
	}
	if len(req.LibraryRoots) == 0 {
		if deps.Hub != nil {
			deps.Hub.Publish("setup.paths.validation.error", map[string]any{
				"error":   "missing_library_roots",
				"message": "at least one library root is required",
			})
		}
		return SetupPathsValidateResponse{}, errors.New("at least one library root is required")
	}

	if deps.Hub != nil {
		deps.Hub.Publish("setup.paths.validation.progress", map[string]any{
			"step":   "checking_downloads_completed",
			"path":   dl,
			"status": "validating",
		})
	}
	dlCheck := checkDirRW(ctx, dl, dryRun)

	if deps.Hub != nil {
		deps.Hub.Publish("setup.paths.validation.progress", map[string]any{
			"step":   "checking_quarantine",
			"path":   q,
			"status": "validating",
		})
	}
	qCheck := checkDirRW(ctx, q, dryRun)

	roots := make([]PathCheck, 0, len(req.LibraryRoots))
	for i, r := range req.LibraryRoots {
		if deps.Hub != nil {
			deps.Hub.Publish("setup.paths.validation.progress", map[string]any{
				"step":   "checking_library_root",
				"path":   r,
				"index":  i + 1,
				"total":  len(req.LibraryRoots),
				"status": "validating",
			})
		}
		rp := cleanPath(r)
		if rp == "" {
			roots = append(roots, PathCheck{Path: r, Exists: false, Errors: []string{"empty path"}})
			continue
		}
		roots = append(roots, checkDirRW(ctx, rp, dryRun))
	}

	// Hardlink viability: downloadsCompleted must be on same filesystem as each library root.
	// Only computed when both sides exist.
	for i := range roots {
		if dlCheck.Exists && roots[i].Exists {
			dlDev := dlCheck.FilesystemID
			rootDev := roots[i].FilesystemID
			roots[i].HardlinkViable = dlDev != "" && rootDev != "" && dlDev == rootDev
			if !roots[i].HardlinkViable {
				roots[i].Warnings = append(roots[i].Warnings, "hardlink not viable (different filesystem)")
			}
		}
	}

	// In dry run mode, paths don't need to exist - they'll be validated when actually used
	ok := true
	if !dryRun {
		ok = dlCheck.Exists && dlCheck.IsDir && dlCheck.Readable && dlCheck.Writable &&
			qCheck.Exists && qCheck.IsDir && qCheck.Readable && qCheck.Writable
		for i := range roots {
			ok = ok && roots[i].Exists && roots[i].IsDir && roots[i].Readable && roots[i].Writable
		}
	}

	resp := SetupPathsValidateResponse{
		Ok:                 ok,
		DownloadsCompleted: dlCheck,
		LibraryRoots:       roots,
		Quarantine:         qCheck,
	}

	if deps.Hub != nil {
		deps.Hub.Publish("setup.paths.validation.completed", map[string]any{
			"ok":     ok,
			"dryRun": dryRun,
			"summary": map[string]any{
				"downloadsCompletedOk": dlCheck.Exists && dlCheck.IsDir && dlCheck.Readable && dlCheck.Writable,
				"quarantineOk":         qCheck.Exists && qCheck.IsDir && qCheck.Readable && qCheck.Writable,
				"libraryRootsOk":       len(roots) > 0 && allPathsOk(roots),
			},
		})
	}

	return resp, nil
}

func allPathsOk(roots []PathCheck) bool {
	for _, r := range roots {
		if !r.Exists || !r.IsDir || !r.Readable || !r.Writable {
			return false
		}
	}
	return true
}

func cleanPath(p string) string {
	p = strings.TrimSpace(p)
	if p == "" {
		return ""
	}
	p = filepath.Clean(p)
	return p
}

func checkDirRW(ctx context.Context, path string, dryRun bool) PathCheck {
	pc := PathCheck{Path: path}

	// In dry run mode, provide informational messages instead of errors
	if dryRun {
		st, err := os.Stat(path)
		if err != nil {
			pc.Exists = false
			pc.Warnings = append(pc.Warnings, "Path does not exist (will be validated when actually used in dry run mode)")
			return pc
		}
		pc.Exists = true
		pc.IsDir = st.IsDir()
		if !pc.IsDir {
			pc.Warnings = append(pc.Warnings, "Path exists but is not a directory")
			return pc
		}
		pc.Readable = canReadDir(path)
		pc.Writable = canWriteDir(path)
		if !pc.Readable {
			pc.Warnings = append(pc.Warnings, "Directory exists but is not readable")
		}
		if !pc.Writable {
			pc.Warnings = append(pc.Warnings, "Directory exists but is not writable")
		}
		pc.FilesystemID = filesystemID(path)
		return pc
	}

	// Normal validation for non-dry-run mode
	st, err := os.Stat(path)
	if err != nil {
		pc.Exists = false
		pc.Errors = append(pc.Errors, err.Error())
		return pc
	}
	pc.Exists = true
	pc.IsDir = st.IsDir()
	if !pc.IsDir {
		pc.Errors = append(pc.Errors, "not a directory")
		return pc
	}

	pc.Readable = canReadDir(path)
	pc.Writable = canWriteDir(path)
	if !pc.Readable {
		pc.Errors = append(pc.Errors, "not readable")
	}
	if !pc.Writable {
		pc.Errors = append(pc.Errors, "not writable")
	}

	pc.FilesystemID = filesystemID(path)
	return pc
}

func canReadDir(path string) bool {
	f, err := os.Open(path)
	if err != nil {
		return false
	}
	defer f.Close()
	_, err = f.Readdirnames(1)
	if err == nil || errors.Is(err, os.ErrNotExist) {
		return true
	}
	// If empty dir, Readdirnames returns io.EOF which is fine.
	if errors.Is(err, os.ErrClosed) {
		return false
	}
	if err.Error() == "EOF" {
		return true
	}
	return true
}

func canWriteDir(path string) bool {
	name := ".rivet_write_test"
	p := filepath.Join(path, name)
	b := []byte("ok")
	if err := os.WriteFile(p, b, 0o600); err != nil {
		return false
	}
	_ = os.Remove(p)
	return true
}

func filesystemID(path string) string {
	if runtime.GOOS == "windows" {
		return ""
	}
	var st syscall.Stat_t
	if err := syscall.Stat(path, &st); err != nil {
		return ""
	}
	return "dev:" + itoa64(uint64(st.Dev))
}

func itoa64(n uint64) string {
	// local helper to avoid pulling in fmt.
	if n == 0 {
		return "0"
	}
	buf := make([]byte, 0, 20)
	for n > 0 {
		d := n % 10
		buf = append(buf, byte('0'+d))
		n /= 10
	}
	// reverse
	for i, j := 0, len(buf)-1; i < j; i, j = i+1, j-1 {
		buf[i], buf[j] = buf[j], buf[i]
	}
	return string(buf)
}
