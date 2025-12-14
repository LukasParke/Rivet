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

func RegisterSetupPaths(api fiber.Router, db *sql.DB) {
	grp := api.Group("/setup")

	grp.Post("/paths/validate", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		var req SetupPathsRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 8*time.Second)
		defer cancel()

		resp, err := validatePaths(ctx, req)
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

		resp, err := validatePaths(ctx, req)
		if err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_paths", "message": err.Error()})
		}
		if !resp.Ok {
			return c.Status(http.StatusBadRequest).JSON(resp)
		}

		b, _ := json.Marshal(req)
		if err := storage.UpsertSetting(ctx, db, "setup.paths", string(b)); err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "save_failed"})
		}

		_ = storage.InsertAuditEvent(ctx, db, storage.AuditEvent{Action: "setup.paths_saved"})
		return c.JSON(SetupPathsSaveResponse{Saved: true})
	})
}

func validatePaths(ctx context.Context, req SetupPathsRequest) (SetupPathsValidateResponse, error) {
	dl := cleanPath(req.DownloadsCompletedPath)
	q := cleanPath(req.QuarantinePath)
	if dl == "" {
		return SetupPathsValidateResponse{}, errors.New("downloadsCompletedPath is required")
	}
	if q == "" {
		return SetupPathsValidateResponse{}, errors.New("quarantinePath is required")
	}
	if len(req.LibraryRoots) == 0 {
		return SetupPathsValidateResponse{}, errors.New("at least one library root is required")
	}

	dlCheck := checkDirRW(ctx, dl)
	qCheck := checkDirRW(ctx, q)

	roots := make([]PathCheck, 0, len(req.LibraryRoots))
	for _, r := range req.LibraryRoots {
		rp := cleanPath(r)
		if rp == "" {
			roots = append(roots, PathCheck{Path: r, Exists: false, Errors: []string{"empty path"}})
			continue
		}
		roots = append(roots, checkDirRW(ctx, rp))
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

	ok := dlCheck.Exists && dlCheck.IsDir && dlCheck.Readable && dlCheck.Writable &&
		qCheck.Exists && qCheck.IsDir && qCheck.Readable && qCheck.Writable
	for i := range roots {
		ok = ok && roots[i].Exists && roots[i].IsDir && roots[i].Readable && roots[i].Writable
	}

	return SetupPathsValidateResponse{
		Ok:                 ok,
		DownloadsCompleted: dlCheck,
		LibraryRoots:       roots,
		Quarantine:         qCheck,
	}, nil
}

func cleanPath(p string) string {
	p = strings.TrimSpace(p)
	if p == "" {
		return ""
	}
	p = filepath.Clean(p)
	return p
}

func checkDirRW(ctx context.Context, path string) PathCheck {
	pc := PathCheck{Path: path}

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
