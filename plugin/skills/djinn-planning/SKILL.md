---
name: djinn-planning
description: Plan projects with Djinn. Handles new-project setup (questioning, research, requirements, roadmap), milestone planning (task decomposition with wave ordering), milestone discussion (context gathering, design decisions), and progress checking. Triggers on project initialization, planning, roadmap creation, milestone decomposition, or mentions of djinn planning.
---

# Djinn Planning

Route to the correct planning workflow based on user intent.

## Detect Workflow

Based on `$ARGUMENTS` or conversation context, load ONE of:

| Intent Signal | Workflow | File |
|---------------|----------|------|
| "new project", "start project", "initialize" | New Project | [new-project/SKILL.md](new-project/SKILL.md) |
| "plan milestone N", "plan phase N", number argument | Plan Milestone | [plan-milestone/SKILL.md](plan-milestone/SKILL.md) |
| "discuss milestone N", "scope", "context" | Discuss Milestone | [discuss-milestone/SKILL.md](discuss-milestone/SKILL.md) |
| "progress", "status", "what's next" | Progress | [progress/SKILL.md](progress/SKILL.md) |

Read the matched file and follow its instructions completely. If intent is ambiguous, ask the user which workflow they want.

## Shared Resources

Planning templates and task templates are available as cookbooks. Load them when a workflow references them:

- [cookbook/planning-templates.md](cookbook/planning-templates.md) -- Memory output patterns for all artifact types
- [cookbook/task-templates.md](cookbook/task-templates.md) -- Task hierarchy creation and wave ordering patterns
