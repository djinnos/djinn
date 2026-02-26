# What-If Analysis

Rapidly explore how changes to key variables affect outcomes. Test assumptions, find sensitivities, and prepare for contingencies.

## When to Use

- Testing assumptions in plans or models
- Finding which variables matter most
- Exploring edge cases
- Quick strategic exploration
- Preparing contingency plans
- Time: 15-45 minutes

## The Core Approach

Systematically vary one or more inputs and observe the effect on outputs.

```
Input Variables          Model/Plan          Output Results
┌─────────────┐         ┌─────────┐         ┌─────────────┐
│ Price       │         │         │         │ Revenue     │
│ Volume      │  ────▶  │ Business│  ────▶  │ Profit      │
│ Costs       │         │  Model  │         │ Growth      │
│ Timing      │         │         │         │ Risk        │
└─────────────┘         └─────────┘         └─────────────┘

What if Price goes up 20%?
What if Volume drops 50%?
What if Costs increase 10%?
```

## Types of What-If Analysis

### Single Variable (Sensitivity)
Change one variable, hold others constant.

**Question format**: "What if [variable] changes to [value]?"

**Example**:
- Base case: 1000 customers at $100/month = $100K MRR
- What if customers = 800? → $80K MRR (-20%)
- What if customers = 1200? → $120K MRR (+20%)
- What if price = $80? → $80K MRR (-20%)
- What if price = $120? → $120K MRR (+20%)

### Multi-Variable (Scenario)
Change multiple variables together.

**Question format**: "What if [var1] AND [var2] change?"

**Example**:
- Pessimistic: 800 customers at $80 = $64K MRR (-36%)
- Optimistic: 1200 customers at $120 = $144K MRR (+44%)

### Extreme/Stress Test
Push variables to extremes to find breaking points.

**Question format**: "What if [extreme situation]?"

**Example**:
- What if we lose our biggest customer (30% of revenue)?
- What if costs double overnight?
- What if market shrinks by 50%?

### Reversal
Work backward from desired outcome.

**Question format**: "What would need to be true for [outcome]?"

**Example**:
- What would need to happen to double revenue?
- What would cause us to go bankrupt?
- What would make this project succeed?

## The Process

### Step 1: Define Base Case (5 min)

Document your current assumptions and expected outcomes.

```markdown
## Base Case Assumptions
- Customer growth: 10% per month
- Churn rate: 5% monthly
- Price: $100/user/month
- CAC: $500
- LTV: $2000

## Expected Outcomes (12 months)
- Customers: 1000 → 1800
- Revenue: $100K → $180K MRR
- Profit margin: 20%
```

### Step 2: Identify Key Variables (5 min)

List variables that could significantly affect outcomes:

**Categories**:
- Revenue drivers (price, volume, conversion)
- Cost drivers (fixed, variable, headcount)
- Timing factors (launch date, sales cycle)
- External factors (market size, competition)
- Operational factors (capacity, quality)

### Step 3: Run What-If Scenarios (15-30 min)

For each key variable, test:
- Optimistic case (+20-50%)
- Pessimistic case (-20-50%)
- Extreme case (2x or 0.5x)

**Sensitivity table**:

| Variable | -50% | -20% | Base | +20% | +50% |
|----------|------|------|------|------|------|
| Price | $50 | $80 | $100 | $120 | $150 |
| Revenue impact | -50% | -20% | 0% | +20% | +50% |
| Customers | 500 | 800 | 1000 | 1200 | 1500 |
| Revenue impact | -50% | -20% | 0% | +20% | +50% |
| Costs | $40K | $64K | $80K | $96K | $120K |
| Profit impact | +50% | +20% | 0% | -20% | -50% |

### Step 4: Identify Sensitivities (5 min)

Which variables have the biggest impact?

**Sensitivity ranking**:
1. Customer volume - highest impact
2. Churn rate - high impact (compounds)
3. Price - moderate impact
4. CAC - moderate impact
5. Fixed costs - low impact

Focus planning and monitoring on high-sensitivity variables.

### Step 5: Develop Contingencies (5 min)

For high-risk scenarios, prepare response plans:

| If... | Then... |
|-------|---------|
| Churn exceeds 8% | Trigger customer success intervention |
| Growth below 5% | Increase marketing spend or pivot |
| Costs exceed 90% of revenue | Implement cost reduction plan |

## What-If Template

```markdown
## What-If Analysis: [Subject]

**Date**:
**Base case**:
**Time horizon**:

### Key Variables
| Variable | Base Value | Range to Test |
|----------|------------|---------------|
| | | |
| | | |

### Sensitivity Analysis
| Variable | -50% | -20% | Base | +20% | +50% |
|----------|------|------|------|------|------|
| | | | | | |
| | | | | | |

### Key Sensitivities
1. [Most sensitive variable]: [Impact]
2. [Second most]: [Impact]
3. [Third most]: [Impact]

### Critical Scenarios
| Scenario | Probability | Impact | Response |
|----------|-------------|--------|----------|
| | | | |

### Insights
-
-

### Action Items
-
-
```

## Quick What-If Questions

When you don't have time for full analysis, ask these:

**Optimistic**: "What if everything goes 20% better than expected?"
**Pessimistic**: "What if everything goes 20% worse?"
**Single failure**: "What if [biggest assumption] is wrong?"
**Timing**: "What if this takes twice as long?"
**Scale**: "What if we need to 10x this?"
**Constraint**: "What if [key resource] is unavailable?"

## Combining with Other Techniques

| Technique | How to Combine |
|-----------|----------------|
| SWOT | What-if on threats materializing |
| Scenario Planning | What-if within each scenario |
| Pre-mortem | What-if on failure causes |
| Five Whys | What-if on root cause fixes |

## Tips for Better What-If Analysis

### Test non-obvious variables
Don't just test the obvious (price, volume). Test timing, quality, assumptions about customer behavior.

### Use ranges, not points
"What if price is $80-120?" is more useful than "What if price is $97?"

### Look for non-linear effects
Some variables have thresholds. Going from 5% churn to 6% might be fine; 5% to 15% might be catastrophic.

### Document assumptions
Every what-if depends on assumptions. Make them explicit.

### Connect to action
If this happens, what would we do? Have a response ready.

## Common Mistakes

- **Only testing favorable scenarios**: Test the uncomfortable ones too
- **Ignoring correlations**: Variables often move together (recession = lower volume AND lower prices)
- **False precision**: "Revenue will be $1,234,567" - use ranges
- **Not testing assumptions**: The base case itself might be wrong
- **Analysis without action**: What-if should lead to contingency plans
- **Forgetting timing**: When something happens matters, not just what
