# Research Phase Selection

## The Core Decision Tree

```
                    What do you need to learn?
                           │
         ┌─────────────────┼─────────────────┬─────────────────┐
         ▼                 ▼                 ▼                 ▼
    Who are the       Will this        How should we      Does it meet
    users and what    solution         build/organize     standards?
    do they need?     work for them?   this?
         │                 │                 │                 │
         ▼                 ▼                 ▼                 ▼
      DISCOVERY        VALIDATION         DESIGN           COMPLIANCE
```

## Detailed Decision Flow

```
START: What research question are you answering?
│
├─► Do you understand your users well enough?
│   │
│   ├─► NO: What's missing?
│   │   │
│   │   ├─► Deep motivations/context ────────► Interview Design
│   │   ├─► End-to-end experience ───────────► Journey Mapping
│   │   └─► How they organize info ──────────► Card Sorting
│   │
│   └─► YES: Do you have a solution to test?
│       │
│       ├─► YES: What stage is the solution?
│       │   │
│       │   ├─► Early concept/sketch ────────► Prototype Feedback
│       │   ├─► Working design ──────────────► Usability Testing
│       │   └─► Need scale/numbers ──────────► Survey Design
│       │
│       └─► NO: Need to shape the output?
│           │
│           ├─► Evaluating designs ──────────► Design Critique
│           ├─► Prioritizing features ───────► User Story Mapping
│           └─► Capturing constraints ───────► Requirements Gathering
│
└─► Need to verify compliance/standards? ────► Accessibility Audit
```

## Quick Reference Matrix

| Signal | Phase | Go to |
|--------|-------|-------|
| "We don't understand our users" | Discovery | Interviews, Journey Maps, Card Sorting |
| "What do users actually need?" | Discovery | Interviews |
| "Where are the pain points?" | Discovery | Journey Mapping |
| "How do users think about this?" | Discovery | Card Sorting |
| "Will this design work?" | Validation | Prototypes, Usability, Surveys |
| "Is this concept right?" | Validation | Prototype Feedback |
| "Can users accomplish tasks?" | Validation | Usability Testing |
| "What do users prefer at scale?" | Validation | Survey Design |
| "How should we build this?" | Design | Critique, Story Maps, Requirements |
| "Is this design good?" | Design | Design Critique |
| "What features matter most?" | Design | User Story Mapping |
| "What are the constraints?" | Design | Requirements Gathering |
| "Does it meet accessibility standards?" | Compliance | Accessibility Audit |

## Phase Flow

```
┌──────────────────────────────────────────────────────────────────────┐
│                                                                      │
│   DISCOVERY ────► VALIDATION ────► DESIGN ────► COMPLIANCE          │
│       │               │               │                              │
│       │               │               │                              │
│       └───────────────┴───────────────┘                              │
│              (Loop back when you learn something new)                │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘
```

**Not always linear.** You may loop back:
- Validation reveals users don't understand → Return to Discovery
- Design critique shows usability issues → Return to Validation
- Requirements conflict with user needs → Return to Discovery

## Quick Start by Scenario

| Scenario | Start With | Why |
|----------|------------|-----|
| New product idea | Discovery: Interviews | Understand the problem space first |
| Redesigning existing product | Discovery: Journey Mapping | Map current pain points |
| Reviewing mockups/designs | Validation: Prototype Feedback | Get early reaction before building |
| Pre-launch quality check | Validation: Usability Testing | Verify users can complete tasks |
| Need quantitative data | Validation: Survey Design | Scale beyond individual interviews |
| Organizing navigation/content | Discovery: Card Sorting | Understand user mental models |
| Feature prioritization | Design: User Story Mapping | Connect user goals to features |
| Stakeholder alignment | Design: Requirements Gathering | Capture constraints and trade-offs |
| Evaluating design quality | Design: Design Critique | Structured feedback framework |
| Accessibility gate | Compliance: Accessibility Audit | WCAG compliance check |

## Phase Details

### Phase 1: Discovery
**Goal**: Understand users deeply before building anything.

| Technique | Best For | Time |
|-----------|----------|------|
| Interview Design | Deep motivations, context, "why" | 45-60 min/interview |
| Journey Mapping | End-to-end experience, pain points | 2-4 hours |
| Card Sorting | Mental models, information architecture | 15-30 min/participant |

**You're in Discovery when:**
- Building something new
- Don't know what users actually need
- Need to understand the problem before solving it

### Phase 2: Validation
**Goal**: Test whether your solution works for users.

| Technique | Best For | Time |
|-----------|----------|------|
| Prototype Feedback | Early concepts, interaction design | 15-30 min/participant |
| Usability Testing | Task completion, real product | 30-60 min/participant |
| Survey Design | Quantitative data at scale | Variable |

**You're in Validation when:**
- Have a design/prototype to test
- Need evidence that solution works
- Want to measure success quantitatively

### Phase 3: Design Support
**Goal**: Shape the output with research-informed decisions.

| Technique | Best For | Time |
|-----------|----------|------|
| Design Critique | Structured design feedback | 30-60 min |
| User Story Mapping | Feature prioritization, MVP definition | 2-4 hours |
| Requirements Gathering | Constraints, trade-offs, specs | Variable |

**You're in Design Support when:**
- Evaluating design options
- Prioritizing what to build
- Documenting requirements and constraints

### Phase 4: Compliance
**Goal**: Ensure the product meets standards.

| Technique | Best For | Time |
|-----------|----------|------|
| Accessibility Audit | WCAG compliance, inclusive design | 4-8 hours |

**You're in Compliance when:**
- Preparing for launch
- Legal/regulatory requirements
- Ensuring inclusive access

## Red Flags: Wrong Phase Selected

### Should be Discovery, not Validation
- You're testing but don't know if you're solving the right problem
- Users seem confused about the entire concept
- You realize you don't know who your actual users are

### Should be Validation, not Discovery
- You keep interviewing but never building
- You have strong assumptions but no evidence
- Stakeholders want data, not stories

### Should be Design Support, not Validation
- You're critiquing designs internally, not with users
- You need to prioritize features, not test them
- Focus is on constraints and trade-offs

## After Phase Selection

Once you know your phase, read the relevant technique docs:

- **Discovery**: [cookbook/discovery/](../cookbook/discovery/)
- **Validation**: [cookbook/validation/](../cookbook/validation/)
- **Design**: [cookbook/design/](../cookbook/design/)
- **Compliance**: [cookbook/compliance/](../cookbook/compliance/)
