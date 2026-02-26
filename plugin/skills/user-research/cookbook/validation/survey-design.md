# Survey Design

Create surveys that collect reliable, actionable data at scale. Bad surveys generate bad data - worse than no data because they create false confidence.

## When to Use

- Validating qualitative findings at scale
- Measuring satisfaction, preferences, attitudes
- Segmenting users by behavior or needs
- Prioritizing features or problems
- Benchmarking over time
- Time: 2-4 hours to design, days to collect

## When NOT to Use

| Use Surveys For | Don't Use Surveys For |
|-----------------|----------------------|
| How many, how often | Why (use interviews) |
| Measuring attitudes | Understanding context |
| Validating hypotheses | Discovering unknowns |
| Large sample needs | When you need depth |
| Quantifiable questions | Open exploration |

## Survey Structure

```
┌─────────────────────────────────────────────────────────────┐
│ INTRODUCTION                                                 │
│ - Purpose (why should they care?)                           │
│ - Time estimate                                              │
│ - Privacy/anonymity statement                               │
├─────────────────────────────────────────────────────────────┤
│ SCREENING QUESTIONS (if needed)                             │
│ - Filter to qualified respondents                           │
├─────────────────────────────────────────────────────────────┤
│ WARM-UP QUESTIONS                                           │
│ - Easy, non-threatening                                      │
│ - Build engagement                                           │
├─────────────────────────────────────────────────────────────┤
│ CORE QUESTIONS                                               │
│ - Most important questions                                   │
│ - Group by topic                                             │
│ - Progress from easy to complex                             │
├─────────────────────────────────────────────────────────────┤
│ SENSITIVE/DEMOGRAPHIC QUESTIONS                             │
│ - Save for last                                              │
│ - Make optional if possible                                  │
├─────────────────────────────────────────────────────────────┤
│ CLOSING                                                      │
│ - Thank you                                                  │
│ - Optional open comment                                      │
│ - Next steps (if any)                                        │
└─────────────────────────────────────────────────────────────┘
```

## Question Types

### Multiple Choice (Single Select)
Use when: One answer from defined options

```
How did you first hear about us?
○ Search engine
○ Social media
○ Friend/colleague recommendation
○ Online article or blog
○ Other: _______
```

**Tips**:
- Include "Other" with text field
- Options should be mutually exclusive
- Randomize order to avoid primacy bias

### Multiple Choice (Multi-Select)
Use when: Multiple answers possible

```
Which features do you use regularly? (Select all that apply)
☐ Dashboard
☐ Reports
☐ Integrations
☐ API
☐ Mobile app
```

**Tips**:
- Limit to 7-10 options
- Consider "None of the above"
- Randomize order

### Rating Scale (Likert)
Use when: Measuring agreement, satisfaction, frequency

```
How satisfied are you with our customer support?
○ Very dissatisfied
○ Dissatisfied
○ Neutral
○ Satisfied
○ Very satisfied
```

**Tips**:
- Use consistent scales throughout
- 5-point scales are standard
- Label all points, not just endpoints
- Consider including "N/A" option

### Net Promoter Score (NPS)
Use when: Benchmarking loyalty

```
How likely are you to recommend us to a friend or colleague?
0  1  2  3  4  5  6  7  8  9  10
Not at all likely          Extremely likely

[Optional follow-up]: What's the primary reason for your score?
```

**Scoring**: Promoters (9-10) - Detractors (0-6) = NPS

### Ranking
Use when: Prioritizing options

```
Rank these features by importance (1 = most important):
[ ] Speed
[ ] Ease of use
[ ] Price
[ ] Support
[ ] Integrations
```

**Tips**:
- Limit to 5-7 items
- Consider max-diff for larger sets

### Matrix Questions
Use when: Same scale applies to multiple items

```
Rate your satisfaction with each aspect:
                    Very      Dissatisfied  Neutral  Satisfied  Very
                    Dissatisfied                               Satisfied
Ease of setup         ○           ○           ○          ○         ○
Documentation         ○           ○           ○          ○         ○
Performance           ○           ○           ○          ○         ○
```

**Tips**:
- Limit to 5-7 rows
- Watch for straight-lining (all same answer)

### Open-Ended
Use sparingly: For unexpected insights

```
What's one thing we could do to improve your experience?
[___________________________________]
```

**Tips**:
- Make optional
- Place at end
- Keep to 1-2 open questions max

## Writing Good Questions

### Be specific
❌ "How often do you use our product?"
✅ "In the past 7 days, how many times did you log in?"

### Avoid double-barreled questions
❌ "How satisfied are you with our product's speed and reliability?"
✅ Ask speed and reliability separately

### Avoid leading questions
❌ "How much did you enjoy our new feature?"
✅ "How would you rate the new feature?"

### Avoid loaded language
❌ "Do you agree that our customer service is excellent?"
✅ "How would you rate our customer service?"

### Use simple language
❌ "How frequently do you utilize the dashboard functionality?"
✅ "How often do you use the dashboard?"

### Provide balanced options
❌ "Excellent / Good / Average / Poor" (skewed positive)
✅ "Excellent / Good / Fair / Poor / Very Poor"

## Sample Size

| Population Size | Sample Needed (95% confidence, 5% margin) |
|-----------------|-------------------------------------------|
| 100 | 80 |
| 500 | 217 |
| 1,000 | 278 |
| 10,000 | 370 |
| 100,000 | 383 |
| 1,000,000+ | 385 |

**For segments**: Need sufficient sample in each segment you want to analyze separately.

## Survey Length

| Length | Completion Rate | Best For |
|--------|-----------------|----------|
| 1-3 min (5-10 questions) | 80%+ | Quick pulse checks |
| 5-7 min (15-20 questions) | 60-70% | Standard research |
| 10-15 min (30-40 questions) | 40-50% | Comprehensive studies |
| 15+ min | <40% | Avoid if possible |

**Rule of thumb**: Every additional minute drops completion by 5-10%.

## Survey Design Checklist

### Before writing
- [ ] Clear research objective defined
- [ ] Decision the data will inform identified
- [ ] Target respondents specified
- [ ] Sample size calculated

### Question quality
- [ ] Each question serves the research objective
- [ ] No double-barreled questions
- [ ] No leading questions
- [ ] Simple, clear language
- [ ] Response options are complete and exclusive
- [ ] Consistent scales used throughout

### Survey flow
- [ ] Logical question order
- [ ] Warm-up questions first
- [ ] Sensitive questions last
- [ ] Progress indicator included
- [ ] Estimated time stated

### Testing
- [ ] Pilot tested with 5-10 people
- [ ] Mobile experience verified
- [ ] Skip logic tested
- [ ] Data output reviewed

## Survey Template

```markdown
## Survey Design Document

**Research objective**: [What decision will this inform?]
**Target audience**: [Who should respond?]
**Sample size needed**: [Number]
**Target completion rate**: [%]
**Survey length**: [X minutes, Y questions]

---

### Introduction
[Purpose statement]
[Time estimate]
[Privacy statement]

### Screening Questions (if needed)
Q1: [Question]
- [Options]
[Logic: If X, continue. If Y, disqualify]

### Section 1: [Topic]
Q2: [Question]
- [Options]

Q3: [Question]
- [Options]

### Section 2: [Topic]
[Continue pattern]

### Demographics (optional)
Q[n]: [Question]
- [Options]

### Closing
Thank you message
Optional open comment field

---

### Analysis Plan
- Key metrics to calculate:
- Segments to compare:
- How results will be presented:
```

## Common Mistakes

- **Too long**: Respect people's time. Cut ruthlessly.
- **Leading questions**: Phrasing that suggests "right" answer
- **Missing options**: "Other" and "N/A" often needed
- **Unclear questions**: If you have to explain it, rewrite it
- **No pilot test**: Always test with real people first
- **Wrong audience**: Survey results only as good as respondents
- **Analysis afterthought**: Plan how you'll analyze before launching
- **Survey fatigue**: Don't over-survey the same people
