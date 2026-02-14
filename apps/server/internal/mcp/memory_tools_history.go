package mcp

import (
	"fmt"
	"os/exec"
	"path/filepath"
	"strings"
)

// baseProjectsDir is the allowed base directory for projects
// This should be configured via environment variable or config
var baseProjectsDir = "/home/fernando/git" // default, should be overridden

// SetBaseProjectsDir sets the base directory for project validation
func SetBaseProjectsDir(baseDir string) {
	baseProjectsDir = baseDir
}

// getGitLog retrieves git log for a file with path validation
func getGitLog(projectPath, filePath string, limit int) (string, error) {
	// Validate project path
	if err := validateProjectPath(baseProjectsDir, projectPath); err != nil {
		return "", fmt.Errorf("invalid project path: %w", err)
	}

	// Validate file path
	if err := validateFilePath(filePath); err != nil {
		return "", fmt.Errorf("invalid file path: %w", err)
	}

	// Sanitize arguments
	if err := sanitizeGitArg(filePath); err != nil {
		return "", fmt.Errorf("invalid file argument: %w", err)
	}

	// Build the full file path
	fullPath := filepath.Join(projectPath, filePath)

	// Validate the combined path is still within bounds
	if err := validateProjectPath(baseProjectsDir, fullPath); err != nil {
		return "", fmt.Errorf("combined path outside allowed directory: %w", err)
	}

	// Execute git command safely
	cmd := exec.Command("git", "log", fmt.Sprintf("-%d", limit), "--", fullPath)
	cmd.Dir = projectPath

	output, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("git log failed: %w", err)
	}

	return string(output), nil
}

// getCommitStats retrieves commit statistics with path validation
func getCommitStats(projectPath, commitHash string) (string, error) {
	// Validate project path
	if err := validateProjectPath(baseProjectsDir, projectPath); err != nil {
		return "", fmt.Errorf("invalid project path: %w", err)
	}

	// Sanitize commit hash
	if err := sanitizeGitArg(commitHash); err != nil {
		return "", fmt.Errorf("invalid commit hash: %w", err)
	}

	// Validate commit hash format (should be hex)
	if !isValidCommitHash(commitHash) {
		return "", fmt.Errorf("invalid commit hash format: %s", commitHash)
	}

	cmd := exec.Command("git", "show", "--stat", commitHash)
	cmd.Dir = projectPath

	output, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("git show failed: %w", err)
	}

	return string(output), nil
}

// getGitDiff retrieves git diff between two commits with path validation
func getGitDiff(projectPath, commitHash1, commitHash2 string) (string, error) {
	// Validate project path
	if err := validateProjectPath(baseProjectsDir, projectPath); err != nil {
		return "", fmt.Errorf("invalid project path: %w", err)
	}

	// Sanitize commit hashes
	for _, hash := range []string{commitHash1, commitHash2} {
		if err := sanitizeGitArg(hash); err != nil {
			return "", fmt.Errorf("invalid commit hash: %w", err)
		}
		if !isValidCommitHash(hash) {
			return "", fmt.Errorf("invalid commit hash format: %s", hash)
		}
	}

	cmd := exec.Command("git", "diff", commitHash1, commitHash2)
	cmd.Dir = projectPath

	output, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("git diff failed: %w", err)
	}

	return string(output), nil
}

// getLatestDiff retrieves the latest diff with path validation
func getLatestDiff(projectPath string) (string, error) {
	// Validate project path
	if err := validateProjectPath(baseProjectsDir, projectPath); err != nil {
		return "", fmt.Errorf("invalid project path: %w", err)
	}

	cmd := exec.Command("git", "diff", "HEAD~1", "HEAD")
	cmd.Dir = projectPath

	output, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("git diff failed: %w", err)
	}

	return string(output), nil
}

// getCommitDiff retrieves diff for a specific commit with path validation
func getCommitDiff(projectPath, commitHash string) (string, error) {
	// Validate project path
	if err := validateProjectPath(baseProjectsDir, projectPath); err != nil {
		return "", fmt.Errorf("invalid project path: %w", err)
	}

	// Sanitize commit hash
	if err := sanitizeGitArg(commitHash); err != nil {
		return "", fmt.Errorf("invalid commit hash: %w", err)
	}

	// Validate commit hash format
	if !isValidCommitHash(commitHash) {
		return "", fmt.Errorf("invalid commit hash format: %s", commitHash)
	}

	cmd := exec.Command("git", "show", commitHash)
	cmd.Dir = projectPath

	output, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("git show failed: %w", err)
	}

	return string(output), nil
}

// isValidCommitHash validates that a string is a valid git commit hash
func isValidCommitHash(hash string) bool {
	if hash == "" {
		return false
	}
	// Allow full 40-char hex or short hashes (minimum 4 chars)
	if len(hash) < 4 || len(hash) > 40 {
		return false
	}
	// Check if it's valid hex
	for _, c := range hash {
		if !((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F')) {
			return false
		}
	}
	// Check for path traversal sequences
	if strings.Contains(hash, "..") {
		return false
	}
	return true
}
