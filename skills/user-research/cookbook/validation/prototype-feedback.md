# Prototype Feedback

Gather rapid feedback on low-fidelity prototypes through task-based testing. Prototype feedback validates interaction design before high-fidelity investment.

## When to Use

- Validating interaction concepts early
- Testing navigation and flow
- Comparing design alternatives
- Identifying usability issues quickly
- Before investing in high-fidelity design
- Time: 15-30 minutes per session

## Prototype Fidelity Levels

| Level | What It Looks Like | Best For |
|-------|-------------------|----------|
| **Paper** | Hand-drawn sketches | Very early concepts, brainstorming |
| **Low-fi** | Wireframes, gray boxes | Flow and structure validation |
| **Mid-fi** | Styled wireframes, basic interaction | Interaction patterns |
| **High-fi** | Near-final visuals | Final validation, stakeholder buy-in |

**Rule**: Test at the lowest fidelity that answers your question.

## The Process

### Step 1: Define What You're Testing (5 min)

```markdown
**Prototype Test Plan**
- Prototype: [Description/link]
- Fidelity: [Paper / Low-fi / Mid-fi / High-fi]
- Core question: [What are we trying to learn?]
- Tasks to test: [2-4 realistic user tasks]
```

### Step 2: Prepare Task Scenarios (10 min)

Write realistic scenarios, not instructions:

**Good scenario**:
```
"You want to invite a colleague to collaborate on a project.
Show me how you would do that."
```

**Bad scenario**:
```
"Click the 'Invite' button in the top right corner."
```

**Scenario Template**:
```markdown
Task [N]: [Task Name]

Scenario: "[Context and goal in user's terms]"

Success criteria:
- [ ] User finds [element]
- [ ] User completes [action]
- [ ] User understands [concept]

Watch for:
- Hesitation at [point]
- Confusion about [element]
- Alternative paths tried
```

### Step 3: Run the Session (15-30 min)

**Opening** (2 min):
```
"I'm going to show you an early version of [product/feature].
This is not finished - it's meant to look rough so we can
learn what works and what doesn't.

I'll ask you to complete some tasks. There's no right or wrong way -
I want to see how you naturally approach these.

Please think out loud - tell me what you're looking for,
what you expect to happen, and what you're thinking."
```

**Per Task** (3-5 min each):
1. Read the scenario
2. Ask them to complete the task
3. Observe and take notes
4. Ask follow-up questions

**Key Questions During Tasks**:
```markdown
Expectation:
- "What do you expect to happen when you [click/tap] that?"
- "Where would you look for [feature]?"

Understanding:
- "What do you think [element] does?"
- "What information is this [section] showing you?"

Recovery:
- "What would you do if something went wrong here?"
- "How would you get back to where you started?"

Improvement:
- "What would make this easier?"
- "Is anything missing that you'd expect?"
```

### Step 4: Document Findings (5 min)

Immediately after each session:

```markdown
**Participant**: [ID]
**Date**: [Date]

### Task Results
| Task | Completed | Difficulty | Notes |
|------|-----------|------------|-------|
| Task 1 | Yes/No/Partial | Easy/Med/Hard | [Notes] |
| Task 2 | Yes/No/Partial | Easy/Med/Hard | [Notes] |

### Key Observations
- [Observation 1]
- [Observation 2]

### Quotes
- "[Exact user quote]" (re: [context])

### Issues Found
| Issue | Severity | Location |
|-------|----------|----------|
| [Issue] | High/Med/Low | [Screen/element] |
```

## Prototype Feedback Template

```markdown
## Prototype Feedback Session

**Prototype**: [Name/version]
**Date**: [Date]
**Participant**: [ID or description]
**Facilitator**: [Name]

---

### Pre-Test Questions
- Have you used similar [products/features] before?
- What do you typically use for [related task]?

---

### Task 1: [Task Name]

**Scenario**: "[Full scenario text]"

**Observations**:
- Path taken: [Describe clicks/navigation]
- Hesitations: [Where did they pause?]
- Errors: [What went wrong?]
- Time: [Approximate duration]

**Outcome**: [ ] Success  [ ] Partial  [ ] Failure

**Verbatim quotes**:
- "[Quote 1]"
- "[Quote 2]"

**Severity of issues**: [ ] Critical  [ ] Major  [ ] Minor  [ ] None

---

### Task 2: [Task Name]
[Same structure]

---

### Post-Test Questions

**Overall impression**:
- "On a scale of 1-5, how easy was this to use?"
- "What was the most confusing part?"
- "What did you like most?"

**Expectations**:
- "Did anything work differently than you expected?"
- "What would you change?"

**Response**: [User's answers]

---

### Summary

**What worked well**:
1. [Positive finding]
2. [Positive finding]

**What needs improvement**:
1. [Issue] - Severity: [H/M/L]
2. [Issue] - Severity: [H/M/L]

**Recommendations**:
1. [Action to take]
2. [Action to take]
```

## Remote Prototype Testing

When testing remotely:

**Tools**:
- Figma prototypes (shareable links)
- Maze, UserTesting, Lookback
- Video conferencing with screen share

**Protocol Adjustments**:
```markdown
1. Send prototype link in advance (optional)
2. Ask them to share their screen
3. Remind them to think aloud (harder remotely)
4. Watch their cursor as proxy for attention
5. Record the session (with permission)
```

## Analyzing Multiple Sessions

After 5+ sessions, look for patterns:

```markdown
**Pattern Analysis**

| Finding | Sessions Observed | Impact |
|---------|-------------------|--------|
| [Pattern 1] | 4/5 | High |
| [Pattern 2] | 3/5 | Medium |
| [Pattern 3] | 2/5 | Low |

**Prioritized Issues**:
1. [Most common, highest impact]
2. [Second priority]
3. [Third priority]

**Design Changes**:
| Issue | Proposed Solution | Effort |
|-------|------------------|--------|
| [Issue] | [Solution] | [H/M/L] |
```

## Tips for Better Prototype Testing

### Set Expectations
- "This is rough on purpose"
- "You can't break anything"
- "I didn't design this, so be honest"

### Stay Neutral
- Don't lead: "What do you think?" not "Isn't this button clear?"
- Don't help: Let them struggle (briefly) to see real issues
- Don't defend: Note feedback, don't explain why it's that way

### Focus on Behavior
- What they DO matters more than what they SAY
- Note hesitations, backtracking, confusion
- Actions reveal more than stated preferences

### Test Early, Test Often
- 5 users find 80% of problems
- Test rough concepts before polishing
- Iterate based on findings, then test again

## Common Mistakes

- **Leading questions**: "You found that easy, right?"
- **Testing too late**: When changes are expensive
- **Too many tasks**: Users get fatigued after 4-5 tasks
- **Helping too much**: Rescuing users hides real problems
- **Testing visuals on wireframes**: Low-fi tests structure, not aesthetics
- **Small sample**: 1-2 users isn't enough for patterns
- **No iteration**: Testing without implementing learnings
