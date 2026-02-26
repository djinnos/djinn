#!/usr/bin/env bash
# Bisection script to find which test creates unwanted files/state
#
# Usage: ./find-polluter.sh <file_to_check> <test_pattern> [test_command]
#
# Examples:
#   ./find-polluter.sh '.git' 'src/**/*.test.ts'                    # npm (default)
#   ./find-polluter.sh '.git' './**/*_test.go' 'go test'            # Go
#   ./find-polluter.sh 'temp.db' 'tests/**/test_*.py' 'pytest'      # Python
#   ./find-polluter.sh 'dump.rdb' 'spec/**/*_spec.rb' 'rspec'       # Ruby

set -e

if [ $# -lt 2 ]; then
  echo "Usage: $0 <file_to_check> <test_pattern> [test_command]"
  echo ""
  echo "Examples:"
  echo "  $0 '.git' 'src/**/*.test.ts'                  # npm test (default)"
  echo "  $0 '.git' './**/*_test.go' 'go test'          # Go"
  echo "  $0 'temp.db' 'tests/**/test_*.py' 'pytest'    # Python"
  echo "  $0 'dump.rdb' 'spec/**/*_spec.rb' 'rspec'     # Ruby"
  exit 1
fi

POLLUTION_CHECK="$1"
TEST_PATTERN="$2"
TEST_CMD="${3:-npm test}"

echo "Searching for test that creates: $POLLUTION_CHECK"
echo "Test pattern: $TEST_PATTERN"
echo "Test command: $TEST_CMD"
echo ""

# Get list of test files
TEST_FILES=$(find . -path "$TEST_PATTERN" 2>/dev/null | sort)
TOTAL=$(echo "$TEST_FILES" | grep -c . || echo "0")

if [ "$TOTAL" -eq 0 ]; then
  echo "No test files found matching pattern: $TEST_PATTERN"
  exit 1
fi

echo "Found $TOTAL test files"
echo ""

COUNT=0
for TEST_FILE in $TEST_FILES; do
  COUNT=$((COUNT + 1))

  # Skip if pollution already exists
  if [ -e "$POLLUTION_CHECK" ]; then
    echo "Warning: Pollution already exists before test $COUNT/$TOTAL"
    echo "   Clean it first: rm -rf $POLLUTION_CHECK"
    echo "   Skipping: $TEST_FILE"
    continue
  fi

  echo "[$COUNT/$TOTAL] Testing: $TEST_FILE"

  # Run the test with the specified command
  $TEST_CMD "$TEST_FILE" > /dev/null 2>&1 || true

  # Check if pollution appeared
  if [ -e "$POLLUTION_CHECK" ]; then
    echo ""
    echo "FOUND POLLUTER!"
    echo "   Test: $TEST_FILE"
    echo "   Created: $POLLUTION_CHECK"
    echo ""
    echo "Pollution details:"
    ls -la "$POLLUTION_CHECK"
    echo ""
    echo "To investigate:"
    echo "  $TEST_CMD $TEST_FILE    # Run just this test"
    echo "  cat $TEST_FILE          # Review test code"
    exit 1
  fi
done

echo ""
echo "No polluter found - all tests clean!"
exit 0
