# Usability Testing

Systematically evaluate how well users can accomplish tasks with a product. Usability testing reveals real problems that assumptions and expert reviews miss.

## When to Use

- Validating design decisions with real users
- Identifying usability issues before launch
- Comparing design alternatives (A/B concepts)
- Benchmarking against competitors
- Measuring improvement over time
- Time: 30-60 minutes per session, 5-10 sessions

## Usability Testing Types

| Type | When | What You Learn |
|------|------|----------------|
| **Formative** | During design | What to fix, improve, change |
| **Summative** | After design | How well it performs (metrics) |
| **Moderated** | With facilitator | Rich qualitative data, follow-ups |
| **Unmoderated** | Self-guided | Scale, quantitative metrics |
| **In-person** | Same location | Body language, environment context |
| **Remote** | Different locations | Geographic diversity, convenience |

## The Usability Test Plan

```markdown
## Usability Test Plan

**Product**: [Name/version being tested]
**Test type**: [Moderated/Unmoderated] [Remote/In-person]
**Date(s)**: [Testing dates]

### Research Questions
What are we trying to learn?
1. [Question 1]
2. [Question 2]
3. [Question 3]

### Success Metrics
| Metric | Target | How Measured |
|--------|--------|--------------|
| Task success rate | > 80% | Completed without critical errors |
| Time on task | < 2 min | Task timer |
| Error rate | < 2 errors | Counted per task |
| Satisfaction | > 4/5 | Post-task rating |

### Participants
- Number: [5-10 typical]
- Profile: [Describe target users]
- Recruitment: [How finding participants]
- Compensation: [Incentive if any]

### Tasks (3-5 tasks)
1. [Task 1 - priority]
2. [Task 2 - priority]
3. [Task 3 - priority]

### Schedule
| Session | Date | Time | Participant |
|---------|------|------|-------------|
| 1 | [Date] | [Time] | [Name/ID] |
```

## Writing Usability Tasks

### Good Task Characteristics

| Do | Don't |
|----|-------|
| Realistic scenarios | Abstract commands |
| User's language | Product jargon |
| Goal-focused | Step-by-step instructions |
| Specific enough to measure | Vague exploration |

### Task Template

```markdown
**Task [N]**: [Task Name]

**Scenario**:
"[Realistic context and goal in user's language]"

**Starting point**: [Where user begins]

**Success criteria**:
- [ ] Primary: [Main goal achieved]
- [ ] Secondary: [Additional success indicators]

**Data to collect**:
- Time to complete
- Errors/wrong paths
- Help requests
- Abandonment

**Post-task questions**:
1. "How difficult was that?" (1-5)
2. "What, if anything, was confusing?"
```

### Example Tasks

```markdown
**Task 1: Find and purchase a product**
Scenario: "You want to buy a blue t-shirt in size medium
for under $30. Find one and add it to your cart."

**Task 2: Contact support**
Scenario: "You have a question about your recent order.
Find a way to contact customer support."

**Task 3: Change account settings**
Scenario: "You've moved to a new address. Update your
shipping address in your account."
```

## Running a Moderated Session

### Session Structure (45-60 min)

| Phase | Time | Activities |
|-------|------|------------|
| Welcome | 5 min | Intro, consent, rapport |
| Warm-up | 5 min | Background questions |
| Tasks | 30-40 min | Core usability tasks |
| Debrief | 5-10 min | Overall impressions, questions |
| Wrap-up | 2 min | Thanks, compensation |

### Facilitator Script

**Welcome**:
```
"Thank you for joining today. We're testing [product] to see
how well it works for people like you. You're helping us
improve it.

A few things:
- This is about testing the product, not you
- There are no right or wrong answers
- If something is confusing, that's valuable feedback
- Think out loud - tell me what you're doing and thinking
- You can stop at any time

Any questions before we start?"
```

**During Tasks**:
```
Avoid:
- Leading: "Did you see the button in the corner?"
- Helping: "You need to click here"
- Reacting: "Good!" or showing disappointment

Use instead:
- "What are you looking for?"
- "What do you think that does?"
- "What would you expect to happen?"
- "Tell me more about that"
```

**When User Is Stuck**:
```
Wait 30 seconds, then:
1. "What are you thinking?"
2. "Where would you expect to find that?"
3. "Is there anything else you'd try?"

If truly stuck after 2 minutes:
"Let's move on to the next task."
(Note the failure and where they got stuck)
```

## Metrics and Measurement

### Core Usability Metrics

| Metric | Definition | Good Benchmark |
|--------|------------|----------------|
| **Task success** | % completing without critical error | > 78% |
| **Time on task** | Seconds to complete | Compare to baseline |
| **Errors** | Wrong clicks, backtracking | < 2 per task |
| **Satisfaction** | Post-task rating (1-5) | > 4.0 |
| **SUS Score** | System Usability Scale | > 68 |

### System Usability Scale (SUS)

Standard 10-question survey for overall usability:

```
Rate 1 (Strongly disagree) to 5 (Strongly agree):

1. I think I would like to use this system frequently
2. I found the system unnecessarily complex
3. I thought the system was easy to use
4. I would need support of a technical person to use this
5. I found the various functions were well integrated
6. I thought there was too much inconsistency
7. I imagine most people would learn to use this quickly
8. I found the system very cumbersome to use
9. I felt very confident using the system
10. I needed to learn a lot before I could get going

Scoring: Odd items subtract 1, even items subtract from 5,
multiply sum by 2.5 = SUS score (0-100)
```

## Analysis and Reporting

### Issue Severity Rating

| Severity | Definition | Action |
|----------|------------|--------|
| **Critical** | Prevents task completion | Fix before launch |
| **Major** | Causes significant difficulty | Fix soon |
| **Minor** | Causes slight difficulty | Fix when possible |
| **Cosmetic** | Minor irritation | Nice to fix |

### Findings Template

```markdown
## Usability Finding

**ID**: UX-[number]
**Severity**: [Critical / Major / Minor / Cosmetic]
**Task**: [Which task(s) affected]
**Observed**: [N] of [N] participants

### Description
[What happened - factual observation]

### Impact
[How it affects users - consequences]

### Evidence
- Participant 1: "[Quote or behavior]"
- Participant 3: "[Quote or behavior]"

### Recommendation
[Proposed solution]

### Screenshot/Recording
[Link or image]
```

### Report Structure

```markdown
## Usability Test Report

**Product**: [Name]
**Date**: [Testing dates]
**Participants**: [N]

### Executive Summary
[2-3 sentences: what was tested, key findings, recommendation]

### Key Metrics
| Metric | Result | Target | Status |
|--------|--------|--------|--------|
| Overall task success | X% | 80% | Pass/Fail |
| Average SUS score | X | 68 | Pass/Fail |

### Top Issues (Prioritized)
1. [Critical issue] - [X participants affected]
2. [Major issue] - [X participants affected]
3. [Major issue] - [X participants affected]

### Task-by-Task Results
[Details per task]

### Recommendations
| Priority | Issue | Recommendation | Effort |
|----------|-------|----------------|--------|
| 1 | [Issue] | [Fix] | [H/M/L] |
| 2 | [Issue] | [Fix] | [H/M/L] |

### Appendix
- Full task scripts
- Participant details
- Session recordings
```

## Tips for Better Usability Testing

### Recruit the Right Users
- Match your actual target audience
- Include different experience levels
- Avoid friends, family, colleagues

### Test Real Tasks
- Based on actual user goals
- In realistic contexts
- With realistic data

### Take Good Notes
- Quote exactly what users say
- Note timestamps for key moments
- Capture behavior, not just success/failure

### Act on Findings
- Prioritize by severity and frequency
- Fix issues, then test again
- Share findings with the team

## Common Mistakes

- **Too few participants**: 5 find most issues, fewer misses patterns
- **Leading questions**: "Was that confusing?" → "Tell me about that"
- **Helping too quickly**: Let users struggle to reveal real issues
- **Testing too late**: When changes are expensive
- **Ignoring outliers**: Sometimes one user reveals real edge case
- **Report without action**: Testing is pointless without fixing
