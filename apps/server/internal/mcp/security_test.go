package mcp

import (
	"os"
	"path/filepath"
	"testing"
)

func TestValidateProjectPath(t *testing.T) {
	// Create a temporary base directory for testing
	tmpDir, err := os.MkdirTemp("", "mcp-test-*")
	if err != nil {
		t.Fatalf("failed to create temp dir: %v", err)
	}
	defer os.RemoveAll(tmpDir)

	tests := []struct {
		name        string
		baseDir     string
		projectPath string
		wantErr     bool
		errContains string
	}{
		{
			name:        "valid path within base directory",
			baseDir:     tmpDir,
			projectPath: filepath.Join(tmpDir, "project1"),
			wantErr:     false,
		},
		{
			name:        "path with traversal sequence",
			baseDir:     tmpDir,
			projectPath: filepath.Join(tmpDir, "..") + "/etc/passwd",
			wantErr:     true,
			errContains: "traversal sequence",
		},
		{
			name:        "path escaping base directory",
			baseDir:     tmpDir,
			projectPath: filepath.Join(tmpDir, "subdir/../../../etc"),
			wantErr:     true,
			errContains: "outside allowed directory",
		},
		{
			name:        "absolute path outside base",
			baseDir:     tmpDir,
			projectPath: "/etc/passwd",
			wantErr:     true,
			errContains: "outside allowed directory",
		},
		{
			name:        "valid nested path",
			baseDir:     tmpDir,
			projectPath: filepath.Join(tmpDir, "projects", "myproject"),
			wantErr:     false,
		},
		{
			name:        "path with dotdot slash",
			baseDir:     tmpDir,
			projectPath: "../secret",
			wantErr:     true,
			errContains: "traversal sequence",
		},
		{
			name:        "path with dotdot backslash",
			baseDir:     tmpDir,
			projectPath: "..\\secret",
			wantErr:     true,
			errContains: "traversal sequence",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			err := validateProjectPath(tt.baseDir, tt.projectPath)
			if (err != nil) != tt.wantErr {
				t.Errorf("validateProjectPath() error = %v, wantErr %v", err, tt.wantErr)
				return
			}
			if err != nil && tt.errContains != "" {
				if !contains(err.Error(), tt.errContains) {
					t.Errorf("validateProjectPath() error = %v, should contain %v", err, tt.errContains)
				}
			}
		})
	}
}

func TestValidateFilePath(t *testing.T) {
	tests := []struct {
		name        string
		filePath    string
		wantErr     bool
		errContains string
	}{
		{
			name:     "valid relative path",
			filePath: "src/main.go",
			wantErr:  false,
		},
		{
			name:     "valid single file",
			filePath: "README.md",
			wantErr:  false,
		},
		{
			name:        "path with traversal",
			filePath:    "../etc/passwd",
			wantErr:     true,
			errContains: "traversal sequence",
		},
		{
			name:        "path escaping directory",
			filePath:    "subdir/../../../etc/passwd",
			wantErr:     true,
			errContains: "escapes allowed directory",
		},
		{
			name:        "absolute path",
			filePath:    "/etc/passwd",
			wantErr:     true,
			errContains: "absolute file paths not allowed",
		},
		{
			name:        "path with dotdot",
			filePath:    "..secret",
			wantErr:     true,
			errContains: "traversal sequence",
		},
		{
			name:     "valid nested path",
			filePath: "deep/nested/path/file.txt",
			wantErr:  false,
		},
		{
			name:     "path with dot",
			filePath: "./config.json",
			wantErr:  false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			err := validateFilePath(tt.filePath)
			if (err != nil) != tt.wantErr {
				t.Errorf("validateFilePath() error = %v, wantErr %v", err, tt.wantErr)
				return
			}
			if err != nil && tt.errContains != "" {
				if !contains(err.Error(), tt.errContains) {
					t.Errorf("validateFilePath() error = %v, should contain %v", err, tt.errContains)
				}
			}
		})
	}
}

func TestSanitizeGitArg(t *testing.T) {
	tests := []struct {
		name        string
		arg         string
		wantErr     bool
		errContains string
	}{
		{
			name:    "valid commit hash",
			arg:     "abc123def456",
			wantErr: false,
		},
		{
			name:    "valid file name",
			arg:     "main.go",
			wantErr: false,
		},
		{
			name:        "command injection with semicolon",
			arg:         "abc; rm -rf /",
			wantErr:     true,
			errContains: "dangerous character",
		},
		{
			name:        "command injection with pipe",
			arg:         "abc | cat /etc/passwd",
			wantErr:     true,
			errContains: "dangerous character",
		},
		{
			name:        "command injection with backtick",
			arg:         "abc`whoami`",
			wantErr:     true,
			errContains: "dangerous character",
		},
		{
			name:        "command injection with dollar",
			arg:         "abc$(whoami)",
			wantErr:     true,
			errContains: "dangerous character",
		},
		{
			name:        "command injection with newline",
			arg:         "abc\nmalicious",
			wantErr:     true,
			errContains: "dangerous character",
		},
		{
			name:    "valid branch name with hyphen",
			arg:     "feature-branch",
			wantErr: false,
		},
		{
			name:    "valid path with slash",
			arg:     "src/utils/helpers.go",
			wantErr: false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			err := sanitizeGitArg(tt.arg)
			if (err != nil) != tt.wantErr {
				t.Errorf("sanitizeGitArg() error = %v, wantErr %v", err, tt.wantErr)
				return
			}
			if err != nil && tt.errContains != "" {
				if !contains(err.Error(), tt.errContains) {
					t.Errorf("sanitizeGitArg() error = %v, should contain %v", err, tt.errContains)
				}
			}
		})
	}
}

func TestIsValidCommitHash(t *testing.T) {
	tests := []struct {
		name  string
		hash  string
		valid bool
	}{
		{
			name:  "valid full hash",
			hash:  "aabbccddeeff00112233445566778899aabbccdd",
			valid: true,
		},
		{
			name:  "valid short hash",
			hash:  "abc1234",
			valid: true,
		},
		{
			name:  "valid minimal hash",
			hash:  "abcd",
			valid: true,
		},
		{
			name:  "hash too short",
			hash:  "abc",
			valid: false,
		},
		{
			name:  "hash too long",
			hash:  "aabbccddeeff00112233445566778899aabbccdde",
			valid: false,
		},
		{
			name:  "hash with non-hex characters",
			hash:  "abc123xyz",
			valid: false,
		},
		{
			name:  "empty hash",
			hash:  "",
			valid: false,
		},
		{
			name:  "hash with traversal",
			hash:  "abc../def",
			valid: false,
		},
		{
			name:  "valid hash with uppercase",
			hash:  "ABC123DEF456",
			valid: true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := isValidCommitHash(tt.hash)
			if got != tt.valid {
				t.Errorf("isValidCommitHash(%q) = %v, want %v", tt.hash, got, tt.valid)
			}
		})
	}
}

func TestValidateCommitMessage(t *testing.T) {
	tests := []struct {
		name        string
		message     string
		wantErr     bool
		errContains string
	}{
		{
			name:    "valid message",
			message: "Add new feature",
			wantErr: false,
		},
		{
			name:        "empty message",
			message:     "",
			wantErr:     true,
			errContains: "cannot be empty",
		},
		{
			name:        "whitespace only",
			message:     "   ",
			wantErr:     true,
			errContains: "cannot be empty",
		},
		{
			name:        "command injection with semicolon",
			message:     "Add feature; rm -rf /",
			wantErr:     true,
			errContains: "dangerous pattern",
		},
		{
			name:        "command injection with backticks",
			message:     "Add `whoami` feature",
			wantErr:     true,
			errContains: "dangerous pattern",
		},
		{
			name:        "command injection with dollar",
			message:     "Add $(whoami) feature",
			wantErr:     true,
			errContains: "dangerous pattern",
		},
		{
			name:    "message with emoji",
			message: "Add feature 🚀",
			wantErr: false,
		},
		{
			name:    "message with special chars",
			message: "Fix bug #123: update README.md",
			wantErr: false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			err := validateCommitMessage(tt.message)
			if (err != nil) != tt.wantErr {
				t.Errorf("validateCommitMessage() error = %v, wantErr %v", err, tt.wantErr)
				return
			}
			if err != nil && tt.errContains != "" {
				if !contains(err.Error(), tt.errContains) {
					t.Errorf("validateCommitMessage() error = %v, should contain %v", err, tt.errContains)
				}
			}
		})
	}
}

// contains checks if a string contains a substring
func contains(s, substr string) bool {
	return len(s) >= len(substr) && (s == substr || len(s) > 0 && containsImpl(s, substr))
}

func containsImpl(s, substr string) bool {
	for i := 0; i <= len(s)-len(substr); i++ {
		if s[i:i+len(substr)] == substr {
			return true
		}
	}
	return false
}
