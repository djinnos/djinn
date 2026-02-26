# Persona Analysis

Test ideas against specific user personas to ensure solutions work for real people, not abstract "users." Each persona represents a distinct user type with their own goals, frustrations, and context.

## When to Use

- Designing features or experiences
- Validating that solutions fit actual users
- Identifying gaps in user coverage
- Building empathy for different user types
- Time: 15-30 minutes per persona

## The Process

### Step 1: Define Personas

Create 3-5 distinct personas representing your key user types.

**Persona template**:
```
Name: [Memorable name that captures essence]
Role: [What they do]
Goal: [Primary objective]
Frustration: [Main pain point]
Context: [Relevant background]
Quote: [Something they'd actually say]
```

**Persona differentiation criteria**:
- Technical skill level
- Frequency of use
- Primary use case
- Decision-making power
- Risk tolerance
- Resource constraints

### Step 2: Validate Persona Distinctness

Good personas should:
- Have genuinely different needs
- React differently to the same feature
- Require different communication styles
- Have non-overlapping primary goals

**Red flag**: If all your personas would react the same way, they're not distinct enough.

### Step 3: Test Ideas Against Each Persona

For each persona, evaluate your idea:

**Core questions**:
- "Would [Persona] use this? Why or why not?"
- "What would [Persona] find confusing?"
- "What's missing for [Persona]?"
- "How would [Persona] discover this?"
- "What would make [Persona] recommend this?"

**Scenario walkthrough**:
> "It's Tuesday morning. [Persona] opens the product because [trigger]. They want to [goal]. Walk through what happens."

### Step 4: Identify Coverage Gaps

After testing all personas:

| Persona | Works for them? | What's missing? | Priority |
|---------|-----------------|-----------------|----------|
| [Name] | Yes/Partial/No | [Gap] | H/M/L |

**Questions**:
- Which personas are underserved?
- Are we over-optimizing for one persona?
- Who would we lose with this design?
- What's the minimum to serve each persona?

### Step 5: Prioritize and Adapt

Decide how to handle persona conflicts:
- **Primary persona**: Optimize for them, acceptable for others
- **Must-serve**: All personas need baseline experience
- **Defer**: Some personas can wait for future iteration

## Example Personas

### For a project management tool:

**Persona 1: "Startup Sam"**
- Role: Solo founder wearing all hats
- Goal: Get things out of my head quickly
- Frustration: Tools that require setup before value
- Context: Uses phone often, works irregular hours
- Quote: "I don't have time to learn your system"

**Persona 2: "Manager Maria"**
- Role: Team lead with 8 direct reports
- Goal: Know what everyone's working on
- Frustration: Chasing people for status updates
- Context: In meetings half the day, needs async visibility
- Quote: "Just tell me if we're on track"

**Persona 3: "Process Pete"**
- Role: PMO lead at enterprise company
- Goal: Standardize workflows across teams
- Frustration: Teams doing things inconsistently
- Context: Needs reporting, compliance, audit trails
- Quote: "How do I enforce this across 200 people?"

### Testing a feature: "AI task suggestions"

**Startup Sam**: "Maybe useful? But don't slow me down with suggestions when I'm dumping tasks. Let me turn it off easily."

**Manager Maria**: "Could help with task assignment. But I need to trust it won't suggest wrong things to my team. Can I review before it suggests to others?"

**Process Pete**: "Interesting for productivity metrics. But how does it handle our custom fields? Can it follow our naming conventions? What data is it trained on?"

## Tips for Effective Persona Analysis

### Make personas specific
"Small business owner" is too vague. "Restaurant owner with 3 locations, 20 employees, and no IT staff" is actionable.

### Include edge personas
The power user who pushes limits. The reluctant user forced to use it. The administrator who never uses the product directly.

### Update personas with research
Personas should evolve as you learn more about real users. Static personas become stale.

### Use real quotes when possible
Nothing beats actual user quotes for grounding personas in reality.

## Persona Creation Workshop Format

**If creating personas from scratch**:

1. **Gather inputs** (30 min)
   - User research, support tickets, sales notes
   - List distinct user behaviors observed

2. **Cluster behaviors** (15 min)
   - Group similar users together
   - Name each cluster

3. **Draft personas** (20 min)
   - Fill out template for each cluster
   - Add distinguishing details

4. **Validate distinctness** (10 min)
   - Test: Would they react differently to [feature]?
   - Merge or split as needed

5. **Prioritize** (10 min)
   - Which personas matter most?
   - Who's primary vs. secondary?

## Common Mistakes

- **Too many personas** - 3-5 is manageable, 10 is unusable
- **Demographic-only** - Age/gender matter less than goals/behaviors
- **Unrealistic personas** - Based on imagination rather than research
- **Static personas** - Never updated with new learnings
- **Ignored personas** - Created but never actually used in decisions
