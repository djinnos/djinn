# User Story Mapping

Visualize the user journey and organize features into a prioritized backlog. Story mapping connects user goals to features and reveals what to build first.

## When to Use

- Planning new product or major feature
- Prioritizing a backlog with stakeholder alignment
- Identifying MVP scope
- Communicating product vision to teams
- Finding gaps in user journey coverage
- Time: 45-90 minutes for initial map

## The Story Map Structure

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              USER JOURNEY (Activities)                       │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐           │
│  │ Discover│→ │ Evaluate│→ │  Sign   │→ │ Onboard │→ │   Use   │→ ...      │
│  │         │  │         │  │   Up    │  │         │  │         │           │
│  └─────────┘  └─────────┘  └─────────┘  └─────────┘  └─────────┘           │
├─────────────────────────────────────────────────────────────────────────────┤
│                              USER TASKS (Steps)                              │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐           │
│  │ Search  │  │ Compare │  │ Create  │  │Complete │  │ Create  │           │
│  │ online  │  │ options │  │ account │  │ profile │  │ project │           │
│  ├─────────┤  ├─────────┤  ├─────────┤  ├─────────┤  ├─────────┤           │
│  │ Read    │  │ Try     │  │ Verify  │  │ Invite  │  │ Add     │           │
│  │ reviews │  │ demo    │  │ email   │  │ team    │  │ content │           │
│  ├─────────┤  ├─────────┤  ├─────────┤  ├─────────┤  ├─────────┤           │
│  │ Visit   │  │ Check   │  │ Set     │  │ Watch   │  │ Share   │           │
│  │ website │  │ pricing │  │ password│  │ tutorial│  │ work    │           │
│  └─────────┘  └─────────┘  └─────────┘  └─────────┘  └─────────┘           │
├─────────────────────────────────────────────────────────────────────────────┤
│ MVP ────────────────────────────────────────────────────────────────────────│
│ V1  ────────────────────────────────────────────────────────────────────────│
│ V2  ────────────────────────────────────────────────────────────────────────│
└─────────────────────────────────────────────────────────────────────────────┘
```

## The Process

### Step 1: Define the Persona (10 min)

Who is this story map for?

```markdown
**Persona**: [Name]
**Goal**: [What they're trying to accomplish]
**Context**: [When/where they'll use this]
```

### Step 2: Map the Backbone (15 min)

Identify high-level **activities** (the big steps in the journey):

```
Typical journey stages:
- Awareness / Discovery
- Evaluation / Consideration
- Purchase / Sign-up
- Onboarding / First use
- Regular usage
- Advanced usage / Growth
- Support / Help
- Renewal / Expansion
```

Write each activity on a card, arrange left to right in order.

### Step 3: Add User Tasks (20 min)

Under each activity, list the **tasks** users perform:

**Questions to ask**:
- "What does the user need to do at this stage?"
- "What steps are involved?"
- "What decisions do they make?"

Arrange tasks vertically under each activity, roughly in order.

### Step 4: Slice into Releases (15 min)

Draw horizontal lines to separate priorities:

| Slice | Contents | Criteria |
|-------|----------|----------|
| **MVP** | Minimum to deliver value | Can user complete core goal? |
| **V1** | Complete experience | Smooth, not just functional |
| **V2** | Enhanced experience | Delight, efficiency, edge cases |
| **Later** | Nice to have | Not essential for success |

**For each slice, ask**:
- Can users accomplish their goal with just this slice?
- What's the smallest thing that delivers value?

### Step 5: Validate and Refine (ongoing)

- Review with users: "Does this match how you'd do it?"
- Check with dev: "Is this technically feasible in this order?"
- Adjust based on feedback

## Story Mapping Template

```markdown
## User Story Map

**Persona**: [Name]
**Goal**: [Primary user goal]
**Date**: [Date created]

### Journey Backbone

| Stage 1 | Stage 2 | Stage 3 | Stage 4 | Stage 5 |
|---------|---------|---------|---------|---------|
| [Activity] | [Activity] | [Activity] | [Activity] | [Activity] |

### Tasks by Stage

**Stage 1: [Activity Name]**
- [ ] Task 1.1
- [ ] Task 1.2
- [ ] Task 1.3

**Stage 2: [Activity Name]**
- [ ] Task 2.1
- [ ] Task 2.2

[Continue for each stage...]

### Release Slices

**MVP (Must Have)**
- Stage 1: [Task], [Task]
- Stage 2: [Task]
- Stage 3: [Task], [Task]

**V1 (Should Have)**
- Stage 1: [Task]
- Stage 2: [Task], [Task]
- Stage 4: [Task]

**V2 (Could Have)**
- [Remaining tasks]

### Dependencies & Notes
- [Dependency 1]
- [Risk/assumption]
```

## Workshop Format (90 min)

**Participants**: Product, Design, Engineering, Stakeholders (5-10 people)

| Time | Activity |
|------|----------|
| 10 min | Intro: Explain story mapping, set context |
| 10 min | Define persona and goal |
| 15 min | Map backbone (activities) - group exercise |
| 25 min | Add tasks - individuals then combine |
| 15 min | Slice into releases - discussion |
| 15 min | Review, identify gaps, next steps |

**Materials**:
- Large wall or digital whiteboard
- Sticky notes (different colors for activities vs tasks)
- Markers
- Dot stickers for voting

## Tips for Better Story Maps

### Keep the user visible
Every task should answer: "What is the USER doing?" Not "What is the system doing?"

### Walk the map
Literally walk through the map telling the user's story. Does it flow?

### Find the gaps
- "What happens if [error scenario]?"
- "What about [edge case]?"
- "How does [user type B] differ?"

### Slice thin
The first slice should be embarrassingly small. If you can't ship it in 2 weeks, slice thinner.

### Update regularly
Story maps are living documents. Revisit as you learn.

## Common Mistakes

- **System-centric**: Tasks describe what the system does, not what users do
- **Too detailed too early**: Getting into implementation before understanding flow
- **No prioritization**: Flat map without release slices
- **Single persona**: Ignoring that different users have different journeys
- **Set and forget**: Not updating as requirements evolve
- **Skipping validation**: Assuming the map matches reality

## Story Map vs Other Artifacts

| Artifact | Purpose | Story Map Relationship |
|----------|---------|------------------------|
| User Journey Map | Emotional experience, touchpoints | Story map is more action-focused |
| Product Backlog | Prioritized list of work | Story map gives context to backlog items |
| PRD | Detailed requirements | Story map is visual summary, PRD has details |
| Roadmap | Timeline and milestones | Story map informs roadmap priorities |
