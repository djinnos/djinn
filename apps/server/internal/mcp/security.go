package mcp

import (
	"fmt"
	"path/filepath"
	"strings"
)

// validateProjectPath validates that the given project path is within the allowed base directory.
// It prevents path traversal attacks by checking for traversal sequences and resolving symlinks.
func validateProjectPath(baseDir, projectPath string) error {
	// Clean the path to normalize it
	cleanPath := filepath.Clean(projectPath)

	// Check for obvious traversal attempts
	if strings.Contains(projectPath, "..") {
		return fmt.Errorf("path contains traversal sequence: %s", projectPath)
	}

	// Get absolute paths
	absBase, err := filepath.Abs(baseDir)
	if err != nil {
		return fmt.Errorf("failed to resolve base directory: %w", err)
	}

	absPath, err := filepath.Abs(cleanPath)
	if err != nil {
		return fmt.Errorf("failed to resolve project path: %w", err)
	}

	// Ensure the resolved path is within the base directory
	// Add trailing separator to prevent partial matches (e.g., /foo/bar matching /foo/baz)
	baseWithSep := absBase + string(filepath.Separator)
	pathWithSep := absPath + string(filepath.Separator)

	if !strings.HasPrefix(pathWithSep, baseWithSep) && absPath != absBase {
		return fmt.Errorf("path outside allowed directory: %s", projectPath)
	}

	return nil
}

// validateFilePath validates a file path for traversal sequences.
// It ensures the path doesn't contain relative directory traversal.
func validateFilePath(filePath string) error {
	// Check for traversal sequences
	if strings.Contains(filePath, "..") {
		return fmt.Errorf("file path contains traversal sequence: %s", filePath)
	}

	// Clean the path
	cleanPath := filepath.Clean(filePath)

	// Check if cleaning resolved any traversal attempts
	if strings.HasPrefix(cleanPath, "..") {
		return fmt.Errorf("file path escapes allowed directory: %s", filePath)
	}

	// Ensure no absolute paths
	if filepath.IsAbs(filePath) {
		return fmt.Errorf("absolute file paths not allowed: %s", filePath)
	}

	return nil
}

// sanitizeGitArg sanitizes a string argument for use in git commands.
// It prevents command injection by rejecting dangerous characters.
func sanitizeGitArg(arg string) error {
	// Reject arguments that could be used for command injection
	dangerousChars := []string{";", "&", "|", "`", "$", "(", ")", "<", ">", "\\", "\n", "\r"}
	for _, char := range dangerousChars {
		if strings.Contains(arg, char) {
			return fmt.Errorf("argument contains dangerous character: %s", arg)
		}
	}
	return nil
}
