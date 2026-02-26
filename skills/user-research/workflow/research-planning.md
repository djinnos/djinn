# Research Planning

Before selecting a technique, answer these foundational questions. Good research starts with clear intent.

## The Five Planning Questions

### 1. What decisions will this research inform?

Research without a decision is just curiosity. Be specific:

| Vague | Specific |
|-------|----------|
| "Learn about users" | "Decide whether to build feature X" |
| "Understand pain points" | "Prioritize which 3 problems to solve in v1" |
| "Get feedback" | "Validate if users can complete checkout flow" |

**If you can't name the decision, you're not ready to research.**

### 2. What do we already know or assume?

Document existing knowledge before gathering more:

- **Known facts**: What data/evidence do we have?
- **Assumptions**: What do we believe but haven't verified?
- **Unknowns**: What gaps need filling?

This prevents redundant research and helps you target the right questions.

### 3. Who are the right participants?

Wrong participants = wrong insights. Define your target:

| Criteria | Questions to Answer |
|----------|---------------------|
| **Who** | Current users? Potential users? Non-users? |
| **Behavior** | What behaviors qualify someone? |
| **Experience** | Novice? Expert? Specific use case? |
| **Exclusions** | Who should NOT be included? |

**Minimum participants by technique:**

| Technique | Minimum | Notes |
|-----------|---------|-------|
| Interviews | 5-8 | Until patterns emerge |
| Journey Mapping | 6-10 | Need diverse paths |
| Card Sorting | 15-30 | For statistical validity |
| Usability Testing | 5 | Finds ~85% of issues |
| Surveys | 100+ | For quantitative validity |

### 4. What's the timeline and budget?

Be realistic about constraints:

| Constraint | Impact on Technique Choice |
|------------|---------------------------|
| < 1 week | Guerrilla testing, internal critique |
| 1-2 weeks | Interviews, prototype feedback |
| 2-4 weeks | Usability testing, card sorting |
| 4+ weeks | Full journey mapping, surveys |

**Budget considerations:**
- Participant incentives ($50-200/hour typical)
- Tools (testing platforms, survey software)
- Recruitment costs (if using agencies)

### 5. How will findings be shared?

Research locked in your head doesn't help the team:

| Audience | Format |
|----------|--------|
| Executives | 1-page summary, key decisions |
| Product team | Findings deck, prioritized insights |
| Designers | Personas, journey maps, quotes |
| Developers | Specs, requirements, edge cases |

## Research Question Framework

Before any technique, write your research questions:

```
Primary Question:
What do we need to learn to make [specific decision]?

Secondary Questions:
1. [Supporting question that informs the primary]
2. [Supporting question that informs the primary]
3. [Supporting question that informs the primary]

Success Criteria:
We'll know research succeeded when we can [specific outcome].
```

**Example:**

```
Primary Question:
Why do users abandon the checkout flow at the payment step?

Secondary Questions:
1. What concerns do users have about payment security?
2. Are there missing payment methods users expect?
3. Is the payment form confusing or broken?

Success Criteria:
We'll know research succeeded when we can prioritize 3 fixes
that address the top abandonment reasons.
```

## Synthesis Best Practices

After gathering data, turn it into insights:

### The Insight Formula

```
OBSERVATION + INTERPRETATION = INSIGHT

"Users clicked back 3 times" + "They couldn't find shipping info"
= "Shipping visibility is blocking purchase decisions"
```

### Synthesis Process

1. **Gather raw data** - Notes, recordings, responses
2. **Code themes** - Tag patterns across participants
3. **Cluster findings** - Group related observations
4. **Identify patterns** - What appears repeatedly?
5. **Form insights** - Observation + interpretation
6. **Prioritize** - Which insights matter most for the decision?

### Documenting Findings

Every research output should include:

```markdown
## Research Summary

### Context
- **Decision**: What we're trying to decide
- **Participants**: Who we talked to (N=X)
- **Method**: Technique used

### Key Findings
1. [Insight 1 - with supporting evidence]
2. [Insight 2 - with supporting evidence]
3. [Insight 3 - with supporting evidence]

### Recommendations
- [Action 1]
- [Action 2]

### Open Questions
- [What we still don't know]
```

## Common Planning Mistakes

| Mistake | Fix |
|---------|-----|
| No clear decision | Define what you'll do with findings |
| Wrong participants | Screen carefully, recruit intentionally |
| Leading questions | Ask "what" and "how", not "do you like" |
| Analysis paralysis | Set a deadline for synthesis |
| Insights not shared | Present findings within 1 week |
| Research theater | Only research if it changes decisions |

## Ready to Start?

Once you've answered the five planning questions:

1. Go to [phase-selection.md](./phase-selection.md) to pick your phase
2. Select the appropriate technique for your needs
3. Follow the technique's cookbook guide
