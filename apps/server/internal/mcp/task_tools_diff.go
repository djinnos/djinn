package mcp

import (
	"fmt"
	"os/exec"
	"path/filepath"
	"strings"
)

// getWorktreeDiff retrieves diff from a worktree with path validation
func getWorktreeDiff(worktreePath string) (string, error) {
	// Validate worktree path
	if err := validateProjectPath(baseProjectsDir, worktreePath); err != nil {
		return "", fmt.Errorf("invalid worktree path: %w", err)
	}

	cmd := exec.Command("git", "diff")
	cmd.Dir = worktreePath

	output, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("git diff failed: %w", err)
	}

	return string(output), nil
}

// getWorktreeStatus retrieves status from a worktree with path validation
func getWorktreeStatus(worktreePath string) (string, error) {
	// Validate worktree path
	if err := validateProjectPath(baseProjectsDir, worktreePath); err != nil {
		return "", fmt.Errorf("invalid worktree path: %w", err)
	}

	cmd := exec.Command("git", "status", "--porcelain")
	cmd.Dir = worktreePath

	output, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("git status failed: %w", err)
	}

	return string(output), nil
}

// getWorktreeFileDiff retrieves diff for a specific file in a worktree with path validation
func getWorktreeFileDiff(worktreePath, filePath string) (string, error) {
	// Validate worktree path
	if err := validateProjectPath(baseProjectsDir, worktreePath); err != nil {
		return "", fmt.Errorf("invalid worktree path: %w", err)
	}

	// Validate file path
	if err := validateFilePath(filePath); err != nil {
		return "", fmt.Errorf("invalid file path: %w", err)
	}

	// Sanitize file path
	if err := sanitizeGitArg(filePath); err != nil {
		return "", fmt.Errorf("invalid file argument: %w", err)
	}

	// Build the full file path
	fullPath := filepath.Join(worktreePath, filePath)

	// Validate the combined path is still within bounds
	if err := validateProjectPath(baseProjectsDir, fullPath); err != nil {
		return "", fmt.Errorf("combined path outside allowed directory: %w", err)
	}

	cmd := exec.Command("git", "diff", "--", fullPath)
	cmd.Dir = worktreePath

	output, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("git diff failed: %w", err)
	}

	return string(output), nil
}

// addWorktreeFile stages a file in a worktree with path validation
func addWorktreeFile(worktreePath, filePath string) error {
	// Validate worktree path
	if err := validateProjectPath(baseProjectsDir, worktreePath); err != nil {
		return fmt.Errorf("invalid worktree path: %w", err)
	}

	// Validate file path
	if err := validateFilePath(filePath); err != nil {
		return fmt.Errorf("invalid file path: %w", err)
	}

	// Sanitize file path
	if err := sanitizeGitArg(filePath); err != nil {
		return fmt.Errorf("invalid file argument: %w", err)
	}

	// Build the full file path
	fullPath := filepath.Join(worktreePath, filePath)

	// Validate the combined path is still within bounds
	if err := validateProjectPath(baseProjectsDir, fullPath); err != nil {
		return fmt.Errorf("combined path outside allowed directory: %w", err)
	}

	cmd := exec.Command("git", "add", "--", fullPath)
	cmd.Dir = worktreePath

	if err := cmd.Run(); err != nil {
		return fmt.Errorf("git add failed: %w", err)
	}

	return nil
}

// commitWorktree creates a commit in a worktree with path validation
func commitWorktree(worktreePath, message string) error {
	// Validate worktree path
	if err := validateProjectPath(baseProjectsDir, worktreePath); err != nil {
		return fmt.Errorf("invalid worktree path: %w", err)
	}

	// Validate commit message to prevent injection
	if err := validateCommitMessage(message); err != nil {
		return fmt.Errorf("invalid commit message: %w", err)
	}

	cmd := exec.Command("git", "commit", "-m", message)
	cmd.Dir = worktreePath

	if err := cmd.Run(); err != nil {
		return fmt.Errorf("git commit failed: %w", err)
	}

	return nil
}

// validateCommitMessage validates a commit message for dangerous content
func validateCommitMessage(message string) error {
	// Reject empty messages
	if strings.TrimSpace(message) == "" {
		return fmt.Errorf("commit message cannot be empty")
	}

	// Reject messages that could be used for command injection
	dangerousPatterns := []string{";", "&", "|", "`", "$(", "<(", ">${", "\n--"}
	for _, pattern := range dangerousPatterns {
		if strings.Contains(message, pattern) {
			return fmt.Errorf("commit message contains dangerous pattern: %s", pattern)
		}
	}

	return nil
}

// getWorktreeBranch returns the current branch of a worktree with path validation
func getWorktreeBranch(worktreePath string) (string, error) {
	// Validate worktree path
	if err := validateProjectPath(baseProjectsDir, worktreePath); err != nil {
		return "", fmt.Errorf("invalid worktree path: %w", err)
	}

	cmd := exec.Command("git", "rev-parse", "--abbrev-ref", "HEAD")
	cmd.Dir = worktreePath

	output, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("git rev-parse failed: %w", err)
	}

	return strings.TrimSpace(string(output)), nil
}
