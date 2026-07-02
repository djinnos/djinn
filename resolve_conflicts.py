#!/usr/bin/env python3
"""Resolve merge conflicts in proposal_tools.rs by keeping both sides."""

import re

filepath = "/workspace/.tmpKjG9tO/server/crates/djinn-control-plane/src/tools/proposal_tools.rs"

with open(filepath, 'r') as f:
    lines = f.readlines()

# Parse the file into segments: non-conflict lines and conflict regions
segments = []
i = 0
current_non_conflict = []

while i < len(lines):
    line = lines[i]
    if line.startswith('<<<<<<< HEAD'):
        # Save accumulated non-conflict lines
        if current_non_conflict:
            segments.append(('normal', current_non_conflict))
            current_non_conflict = []
        
        # Collect HEAD side
        head_lines = []
        i += 1
        while i < len(lines) and not lines[i].startswith('======='):
            head_lines.append(lines[i])
            i += 1
        
        # Skip ======= marker
        i += 1
        
        # Collect origin/main side
        main_lines = []
        while i < len(lines) and not lines[i].startswith('>>>>>>> origin/main'):
            main_lines.append(lines[i])
            i += 1
        
        # Skip >>>>>>> marker
        i += 1
        
        segments.append(('conflict', head_lines, main_lines))
    else:
        current_non_conflict.append(line)
        i += 1

if current_non_conflict:
    segments.append(('normal', current_non_conflict))

# Now reconstruct: keep both sides from conflicts
output = []

# Track which conflict we're in to handle correctly
# We need to combine: signoff_readiness_tests (HEAD) + end_to_end_planner_refinement_loop_tests (main)

# Strategy: for each conflict, emit HEAD side first, then the normal lines, then the main side
# But we need to be smarter - the normal lines between conflicts serve as continuation for both sides

# Let me look at the specific structure:
# Conflict 1: HEAD = signoff_tests module start; MAIN = e2e_tests module start
# Shared 1: continuation of create() (status: None, body_format: None, }) .await .unwrap();)
# Conflict 2: HEAD = signoff test cont; MAIN = e2e test cont
# Shared 2: continuation (}) .await .unwrap();)  
# Conflict 3: HEAD = signoff assertions; MAIN = e2e tests rest

# The signoff module from HEAD needs: Conflict1-HEAD + Shared1 + Conflict2-HEAD + Shared2 + Conflict3-HEAD + closing braces
# The e2e module from main needs: Conflict1-MAIN + Shared1 + Conflict2-MAIN + Shared2 + Conflict3-MAIN + closing braces

# Let me extract the content properly

head_module = []  # signoff_readiness_tests
main_module = []  # end_to_end_planner_refinement_loop_tests

conflict_count = 0
for seg in segments:
    if seg[0] == 'normal':
        normal_lines = seg[1]
        if conflict_count > 0 and conflict_count < 4:
            # These shared lines go into BOTH modules
            head_module.extend(normal_lines)
            main_module.extend(normal_lines)
        # The closing braces (after conflict 3) need special handling
        # Lines 5212-5213: "    }\n" and "}\n" - these close each module
        # But they should only appear once per module at the end
    elif seg[0] == 'conflict':
        conflict_count += 1
        head_lines = seg[1]
        main_lines = seg[2]
        head_module.extend(head_lines)
        main_module.extend(main_lines)

# Now head_module contains the signoff test module body
# and main_module contains the e2e test module body
# Each needs its own closing braces

# The closing "    }\n}\n" was in the shared normal segment after conflict 3
# We need to add them to each module

print(f"Conflict count: {conflict_count}")
print(f"Head module lines: {len(head_module)}")
print(f"Main module lines: {len(main_module)}")

# Build the final output: everything before conflicts + head_module + closing + main_module + closing
final_output = []

# Everything before first conflict (non-conflict segments before conflict_count > 0)
pre_conflict = []
for seg in segments:
    if seg[0] == 'normal':
        # Check if this is before the first conflict
        # We need to be smarter about this
        break

# Actually, let me just rebuild the whole file properly
final_lines = []
seen_conflicts = 0
added_head_closing = False

for seg in segments:
    if seg[0] == 'normal':
        normal_lines = seg[1]
        if seen_conflicts == 0:
            # Before any conflict - just emit normally
            final_lines.extend(normal_lines)
        elif seen_conflicts >= 1 and seen_conflicts <= 3:
            # Between conflicts - emit the shared continuation as part of both modules
            # These lines have already been incorporated into head_module and main_module
            # So we skip them here, and handle them through the module reconstruction
            pass
        elif seen_conflicts > 3:
            # After all conflicts
            final_lines.extend(normal_lines)
    elif seg[0] == 'conflict':
        seen_conflicts += 1

# Now I realize this approach is getting complex. Let me try a different, simpler approach.
# I'll just construct the final file directly.

# First, get all lines before the first conflict marker
pre_conflict_end = 0
for idx, line in enumerate(lines):
    if line.startswith('<<<<<<< HEAD'):
        pre_conflict_end = idx
        break

# Get the head_module and main_module content
# head_module ends with the HEAD side of conflict 3 (assertions)
# main_module ends with the MAIN side of conflict 3 (rest of e2e tests)

# The closing braces were in the post-conflict shared lines
# Line 5212: "    }\n" closes the last test function
# Line 5213: "}\n" closes the module

# For signoff module: head_module already has all test functions except the closing of the last one and module close
# For e2e module: main_module already has all test functions except the closing of the last one and module close

# Actually wait - let me check: does the last test function in each module need its closing brace from the shared lines?

# The HEAD conflict 3 content (signoff assertions) ends with:
#         assert!(signoffs.is_empty(), "no sign-offs should be recorded");
# Then the shared "    }\n" closes the test fn, and "}\n" closes the module

# The MAIN conflict 3 content (e2e tests) ends with:
#         assert!(exported_ids.contains("tradeoffs"));
# Then the shared "    }\n" closes the test fn, and "}\n" closes the module

# So I need to add those closing braces to each module

# Write the resolved file
with open(filepath, 'w') as f:
    # Write everything before the first conflict
    for line in lines[:pre_conflict_end]:
        f.write(line)
    
    # Write the signoff readiness tests module (HEAD)
    for line in head_module:
        f.write(line)
    
    # Add closing braces for the signoff module
    f.write("    }\n")
    f.write("}\n")
    f.write("\n")
    
    # Write the end-to-end planner refinement loop tests module (origin/main)
    for line in main_module:
        f.write(line)
    
    # Add closing braces for the e2e module
    f.write("    }\n")
    f.write("}\n")

print("Conflict resolution complete!")
print(f"Total lines in head_module: {len(head_module)}")
print(f"Total lines in main_module: {len(main_module)}")
