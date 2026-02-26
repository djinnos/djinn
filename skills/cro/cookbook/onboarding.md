# Onboarding CRO

Optimize post-signup onboarding, user activation, first-run experience, or time-to-value.

## Initial Assessment

Before providing recommendations, understand:

1. **Product Context** - What type of product? B2B or B2C? Core value proposition?
2. **Activation Definition** - What's the "aha moment"? What action indicates a user "gets it"?
3. **Current State** - What happens after signup? Where do users drop off?

---

## Core Principles

### 1. Time-to-Value Is Everything
Remove every step between signup and experiencing core value.

### 2. One Goal Per Session
Focus first session on one successful outcome. Save advanced features for later.

### 3. Do, Don't Show
Interactive > Tutorial. Doing the thing > Learning about the thing.

### 4. Progress Creates Motivation
Show advancement. Celebrate completions. Make the path visible.

---

## Defining Activation

### Find Your Aha Moment

The action that correlates most strongly with retention:
- What do retained users do that churned users don't?
- What's the earliest indicator of future engagement?

**Examples by product type:**
- Project management: Create first project + add team member
- Analytics: Install tracking + see first report
- Design tool: Create first design + export/share
- Marketplace: Complete first transaction
- Communication: Send first message + get reply
- Collaboration: Invite team member + collaborate on document

### Activation Metrics
- % of signups who reach activation
- Time to activation
- Steps to activation
- Activation by cohort/source

---

## Onboarding Flow Design

### Immediate Post-Signup (First 30 Seconds)

| Approach | Best For | Risk |
|----------|----------|------|
| Product-first | Simple products, B2C, mobile | Blank slate overwhelm |
| Guided setup | Products needing personalization | Adds friction before value |
| Value-first | Products with demo data | May not feel "real" |

**Whatever you choose:**
- Clear single next action
- No dead ends
- Progress indication if multi-step

### Onboarding Checklist Pattern

**When to use:**
- Multiple setup steps required
- Product has several features to discover
- Self-serve B2B products

**Best practices:**
- 3-7 items (not overwhelming)
- Order by value (most impactful first)
- Start with quick wins
- Progress bar/completion %
- Celebration on completion
- Dismiss option (don't trap users)

**Checklist item structure:**
- Clear action verb
- Expected time/effort
- Why it matters (benefit)
- Visual completion state

### Empty States

Empty states are onboarding opportunities, not dead ends.

**Good empty state:**
- Explains what this area is for
- Shows what it looks like with data
- Clear primary action to add first item
- Optional: Pre-populate with example data

**Empty state copy formula:**
```
[What this is for]
[What it will look like when populated]
[Clear CTA to add first item]
```

### Tooltips and Guided Tours

**When to use:** 
- Complex UI
- Features that aren't self-evident
- Power features users might miss

**Best practices:**
- Max 3-5 steps per tour
- Dismissable at any time
- Don't repeat for returning users
- Contextual (triggered by user action)
- Progressive (reveal as needed)

### Product Tours vs. Contextual Guidance

**Product tours (avoid overuse):**
- Linear walkthrough of features
- Often skipped or dismissed
- User not ready to learn

**Contextual guidance (prefer):**
- Appears when relevant
- Just-in-time learning
- Tied to user's current task

---

## Personalization and Segmentation

### When to Ask Questions

**At signup (use sparingly):**
- Only if immediately needed
- Affects first experience significantly
- 1-2 questions max

**During onboarding (better):**
- After they've seen some value
- When context makes question relevant
- Can affect their specific path

### Segmentation Questions

**Good questions:**
- Role/job function (affects which features to highlight)
- Primary goal/use case (affects success metrics)
- Team size (affects collaboration features)
- Experience level (affects guidance depth)

**Keep questions:**
- Single-choice when possible
- Limited options (4-6 max)
- With clear "Other" escape hatch

---

## Multi-Channel Onboarding

### Email + In-App Coordination

**Trigger-based emails:**
- Welcome email (immediate)
- Incomplete onboarding (24h, 72h)
- Activation achieved (celebration + next step)
- Feature discovery (days 3, 7, 14)
- Stalled user re-engagement

**Email should:**
- Reinforce in-app actions, not duplicate them
- Drive back to product with specific CTA
- Be personalized based on actions taken
- Have clear single purpose per email

### In-App Notifications

**Use for:**
- Celebrating milestones
- Suggesting next steps
- Introducing new features
- Re-engaging after absence

**Avoid:**
- Too frequent notifications
- Generic messages
- Blocking critical workflows

---

## Handling Stalled Users

### Detection
Define "stalled" criteria:
- X days inactive
- Incomplete setup
- Not reached activation

### Re-engagement Tactics

1. **Email sequence**
   - Reminder of value
   - Address potential blockers
   - Offer help/support

2. **In-app recovery**
   - Welcome back message
   - Pick up where left off
   - Simplified path to value

3. **Human touch**
   - For high-value accounts
   - Personal outreach
   - Offer onboarding call

### Re-engagement Email Sequence

**Email 1 (24-48h):** Helpful reminder
- "Picking up where you left off"
- Direct link to next step
- No pressure

**Email 2 (72h):** Value reminder
- Highlight key benefit
- Success story/social proof
- Clear CTA

**Email 3 (7 days):** Help offer
- "Need help getting started?"
- Resource links
- Support contact

**Email 4 (14 days):** Last attempt
- Direct ask: "Still interested?"
- What they're missing
- Easy unsubscribe

---

## Measurement

### Key Metrics

| Metric | Description |
|--------|-------------|
| Activation rate | % reaching activation event |
| Time to activation | How long to first value |
| Onboarding completion | % completing setup |
| Day 1/7/30 retention | Return rate by timeframe |
| Feature adoption | % using key features |

### Funnel Analysis

Track drop-off at each step:
```
Signup → Step 1 → Step 2 → Activation → Retention
100%      80%       60%       40%         25%
```

Identify biggest drops and focus there.

### Cohort Analysis

Compare:
- Activated vs. non-activated retention
- Fast activators vs. slow activators
- Different onboarding paths
- Traffic sources

---

## Common Patterns by Product Type

| Product Type | Key Activation Steps | Focus |
|--------------|---------------------|-------|
| B2B SaaS | Setup wizard → First value action → Team invite → Deep setup | Time-to-value, team adoption |
| Marketplace | Complete profile → Browse → First transaction → Repeat loop | Trust building, first transaction |
| Mobile App | Permissions → Quick win → Push setup → Habit loop | Permission handling, habit formation |
| Content Platform | Follow/customize → Consume → Create → Engage | Personalization, first creation |
| Collaboration | Invite team → First shared action → Establish workflow | Team adoption, shared value |

---

## Output Format

### Onboarding Audit
For each issue: 
- **Finding**: What's wrong
- **Impact**: Effect on activation
- **Recommendation**: Specific fix
- **Priority**: High/Medium/Low

### Onboarding Flow Design
- Activation goal definition
- Step-by-step flow
- Checklist items (if applicable)
- Empty state copy
- Email sequence triggers
- Metrics plan

---

## Experiment Ideas

### Flow Structure Experiments

**Onboarding Approach**
- Product-first vs. guided setup
- With checklist vs. without
- Linear flow vs. choose-your-path
- Required steps vs. skippable

**Setup Wizard**
- Number of steps (3 vs. 5 vs. 7)
- Step order optimization
- Required vs. optional steps
- Progress indicator style

**First Action**
- Template start vs. blank start
- Sample data vs. empty state
- Guided creation vs. free exploration

---

### Progress & Motivation Experiments

**Progress Indicators**
- Checklist vs. progress bar vs. both
- Percentage complete vs. steps remaining
- Gamification (points, badges)
- Celebration moments

**Guidance Depth**
- Minimal hints vs. detailed guidance
- Video tutorials vs. interactive guides
- Tooltips vs. slideouts vs. modals
- One-time vs. persistent help

---

### Personalization Experiments

**Segmentation**
- Role-based onboarding paths
- Use case customization
- Experience level adaptation
- Team size variations

**Content Personalization**
- Industry-specific examples
- Goal-based feature highlighting
- Custom success metrics

---

### Activation Optimization

**Time-to-Value**
- Reduce steps to first value
- Pre-populate with templates
- Skip optional setup
- Defer non-essential config

**Activation Events**
- Different activation definitions
- Single vs. compound activation
- Time-bound activation goals

---

### Re-engagement Experiments

**Stalled User Recovery**
- Email timing (24h vs. 48h vs. 72h)
- Email content (value vs. help offer)
- In-app prompts on return
- Personal outreach threshold

**Return User Experience**
- Welcome back messaging
- Resume vs. restart options
- Simplified re-onboarding

---

## Task-Specific Questions

1. What action most correlates with retention?
2. What happens immediately after signup?
3. Where do users currently drop off?
4. What's your activation rate target?
5. Do you have cohort analysis on successful vs. churned users?
6. What onboarding emails exist today?
