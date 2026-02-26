# Pre-mortem Analysis

Imagine the project has failed completely, then work backward to identify what went wrong. By vividly imagining failure, you surface risks that optimism blinds you to.

## When to Use

- Before starting significant projects
- At major decision points
- When team is overly confident
- Before committing significant resources
- Time: 20-40 minutes

## Why Pre-mortems Work

**Post-mortems** analyze failure after it happens - useful but too late.

**Pre-mortems** create psychological permission to voice concerns. Instead of asking "what could go wrong?" (which triggers defensiveness), you say "it failed - why?" This reframes criticism as analysis.

Research shows pre-mortems increase ability to identify reasons for future outcomes by 30%.

## The Process

### Step 1: Set the Scene (2 min)

Transport yourself to the future where the project has failed.

**Setup statement**:
> "It's [date 6-12 months out]. The project has failed completely. Not partially - completely. It's being shut down as a total failure. We're here to understand why."

**Key mindset shifts**:
- Failure is a fact, not a possibility
- We're analyzing, not preventing (yet)
- All concerns are valid data

### Step 2: Individual Failure Brainstorm (10 min)

Each person (or you, if solo) writes reasons for failure.

**Prompt**:
> "What went wrong? Write as many reasons as you can think of. Be specific."

**Categories to consider**:
- **Technical failures**: Technology didn't work, integration broke, performance issues
- **Team failures**: Lost key people, skill gaps, communication breakdown
- **Resource failures**: Ran out of money, time, or people
- **Market failures**: Wrong timing, customers didn't want it, competition won
- **Execution failures**: Scope creep, missed deadlines, quality issues
- **External failures**: Regulatory changes, economic shifts, dependencies failed

**Format**: Write complete statements
- Not: "timeline"
- But: "We underestimated integration complexity and missed the launch window"

### Step 3: Share and Expand (10 min)

Collect all failure reasons without filtering.

**Rules**:
- No debating whether something is "realistic"
- Add to others' ideas ("Yes, and that probably led to...")
- Encourage the uncomfortable ones
- The concerns people are hesitant to voice are often most valuable

**Expansion prompts**:
- "What else could have gone wrong?"
- "What's the failure mode nobody wants to mention?"
- "What did we assume that turned out to be wrong?"

### Step 4: Cluster and Prioritize (10 min)

Group failure modes by theme and assess.

**Priority matrix**:
| Failure Mode | Likelihood | Impact | Detection | Priority |
|--------------|------------|--------|-----------|----------|
| [Description] | H/M/L | H/M/L | Easy/Hard | 1-5 |

**Priority scoring**:
- High likelihood + High impact + Hard to detect = P1
- Low likelihood + Low impact + Easy to detect = P5

### Step 5: Create Prevention Plan (10 min)

For top priority failure modes, define prevention:

**For each P1/P2 failure mode**:
- **Early warning signs**: How would we know this is happening?
- **Prevention actions**: What can we do now to reduce likelihood?
- **Contingency plan**: If it happens anyway, what's our response?
- **Owner**: Who's responsible for monitoring and prevention?

## Example Pre-mortem

**Project**: Launching new mobile app in 6 months

**Failure Statement**: "It's January. The app launch was a disaster. We're pulling it from the store. What happened?"

**Failure Reasons Generated**:
1. "Performance was terrible on older devices - we only tested on new phones"
2. "Apple rejected us twice for privacy policy issues, delaying launch"
3. "Backend team was pulled to support the main product crisis in October"
4. "We launched during the holiday freeze when nobody could fix bugs"
5. "Key features were cut at the last minute, leaving a hollow product"
6. "QA was compressed to two weeks and missed critical bugs"
7. "We assumed the existing API would work but it couldn't handle mobile patterns"
8. "The designer quit in month 3 and we never found a replacement"

**Priority Assessment**:
| Failure Mode | Likelihood | Impact | Priority |
|--------------|------------|--------|----------|
| Backend team pulled away | High | High | P1 |
| Performance on old devices | Medium | High | P1 |
| QA compression | High | Medium | P2 |
| App store rejection | Medium | Medium | P2 |
| Holiday launch timing | Low | High | P2 |

**Prevention Plan** (for P1: Backend team pulled):
- Early warning: Any escalation on main product
- Prevention: Get explicit commitment from leadership on team allocation
- Contingency: Identify backup engineers, document architecture heavily
- Owner: Project lead, checkpoint at each sprint

## Tips for Effective Pre-mortems

### Make failure vivid
Don't say "the project might fail." Say "The project has failed. The team is demoralized. The company lost $2M. Your boss is asking what happened."

### Include embarrassing failures
"We failed because we ignored obvious warning signs everyone saw but nobody mentioned"

### Time pressure helps
Rapid generation surfaces intuitive concerns before the rational mind filters them.

### Revisit periodically
Pre-mortems at project start, then again at major milestones as new risks emerge.

## Solo Pre-mortem

When doing alone:
1. Write the failure scenario in detail (make it vivid)
2. Set 10-minute timer for failure reasons
3. Don't stop to evaluate - just list
4. After timer, review and prioritize
5. Create prevention plan for top 3

## Common Mistakes

- **Not committing to failure**: Hedging with "might" instead of "did"
- **Focusing only on external factors**: "The market changed" vs. "We didn't adapt"
- **Stopping at surface reasons**: "Timeline slipped" vs. "We underestimated X because Y"
- **Skipping the prevention plan**: Analysis without action is just anxiety
- **Only doing once**: Risks change as projects evolve
