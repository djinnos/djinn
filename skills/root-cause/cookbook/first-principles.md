# First Principles Decomposition

Strip away assumptions and conventions to find fundamental truths, then rebuild solutions from basics. Instead of reasoning by analogy ("how do others do it?"), reason from foundational truths ("what must be true?").

## When to Use

- Current approaches feel fundamentally wrong
- Industry "best practices" aren't working for you
- Want to innovate rather than iterate
- Challenging "that's just how it's done"
- Time: 30-60 minutes for thorough analysis

## The Process

### Step 1: State What You're Examining

Identify the solution, approach, or assumption to decompose.

**Examples**:
- "We need to hire more developers to go faster"
- "Enterprise software requires a sales team"
- "Mobile apps need to be native for good performance"
- "We must raise prices to increase revenue"

### Step 2: List All Assumptions

Write every assumption embedded in the current approach.

**Questions to surface assumptions**:
- "Why do we believe this is necessary?"
- "What are we taking for granted?"
- "What would an outsider find strange about this?"
- "What would we have to explain to someone who knows nothing?"

**Example decomposition**:
Statement: "We need to hire more developers to go faster"

Assumptions:
1. More people = faster progress
2. Bottleneck is developer capacity
3. New developers will be productive quickly
4. Current developers are working on the right things
5. The work requires developers specifically
6. Going faster is the right goal
7. Hiring is the only way to add capacity

### Step 3: Challenge Each Assumption

For each assumption, ask: "Is this actually true? What evidence supports it?"

**Challenge framework**:
| Assumption | Evidence For | Evidence Against | Verdict |
|------------|--------------|------------------|---------|
| [Assumption] | [Support] | [Counter] | True/False/Uncertain |

**Types of challenges**:
- **Empirical**: Is there data showing this is true?
- **Logical**: Does this necessarily follow?
- **Historical**: Has this always been true? Will it remain true?
- **Contextual**: Is this true in our specific situation?

### Step 4: Identify Fundamental Truths

What must be true regardless of assumptions?

**The bedrock questions**:
- "What are the physics-level constraints here?"
- "What cannot be changed no matter what?"
- "If we started from scratch with unlimited resources, what would still be true?"

**Example fundamentals** (for software development):
- Code must be written (or generated)
- Requirements must be understood
- Code must be deployed to be useful
- Users must be able to access the software
- Bugs will exist and must be found

Note: Very few things are truly fundamental. Most "constraints" are actually assumptions.

### Step 5: Rebuild from Fundamentals

Given only the fundamental truths, what solutions emerge?

**Rebuilding questions**:
- "Knowing only the fundamentals, how would we solve this?"
- "What's the simplest solution that satisfies the fundamentals?"
- "What becomes possible without the false assumptions?"

**Example rebuilding**:

Fundamental: "Code must be written or generated"
- What if we generate more code? (AI, code gen tools)
- What if we reduce code needed? (no-code tools, libraries)
- What if we buy instead of build? (existing solutions)

Fundamental: "Requirements must be understood"
- What if requirements are the bottleneck, not dev capacity?
- What if better requirements means less rework?
- What if we're building the wrong things?

New solutions:
- Improve requirements clarity (might be the real bottleneck)
- Adopt more automation/generation
- Buy/integrate instead of building
- Remove features to go faster
- Change what "faster" means

## Example: Electric Vehicles (Tesla's approach)

**Conventional wisdom**: "Electric cars must be expensive because batteries are expensive"

**Assumption decomposition**:
- Battery packs cost $X per kWh (fact at time)
- Battery packs are made of cells, modules, cooling, wiring (fact)
- Battery costs are fixed (ASSUMPTION)
- We must buy batteries from suppliers (ASSUMPTION)
- Battery design is optimized (ASSUMPTION)

**Fundamental truths**:
- Batteries need certain raw materials
- Raw material costs set theoretical floor
- Physics of energy storage is fixed

**Gap analysis**:
- Spot price of raw materials = ~$80/kWh theoretical minimum
- Actual battery cost = ~$600/kWh
- Gap = manufacturing, margins, inefficiency (not physics)

**Rebuilt approach**:
- Vertical integration to remove margins
- Manufacturing innovation to reduce labor
- Design optimization to reduce materials
- Result: Dramatic cost reduction over time

## First Principles in Practice

### For Business Problems
- "We need an office" → What does an office actually provide? (Collaboration, separation of work/home, equipment access)
- "We need meetings" → What do meetings accomplish? (Decisions, alignment, relationship building)
- "We need managers" → What do managers actually do? (Coordination, development, communication, decisions)

### For Technical Problems
- "We need a database" → What do we actually need to store and retrieve?
- "We need microservices" → What problem is microservices solving for us?
- "We need Kubernetes" → What capabilities do we actually require?

### For Product Problems
- "We need feature X" → What job is the user hiring this product to do?
- "Competitors have Y" → Is Y actually valuable or just conventional?
- "Users asked for Z" → What underlying need is Z addressing?

## Tips for First Principles Thinking

### Question experts
Experts often know "how" without questioning "why." They may be the most trapped by convention.

### Look for "everybody knows"
When something is obvious to everyone, it's often an unexamined assumption.

### Find the physics
Actual laws of physics are fundamental. Business "laws" rarely are.

### Test with "why 5 times"
If you can't explain why something must be true after 5 whys, it's probably an assumption.

### Start over mentally
"If we were starting this company/product/team today, would we do it this way?"

## Common Mistakes

- **Calling preferences "fundamentals"**: "Users want pretty UI" is not fundamental
- **Stopping at industry standard**: "That's how everyone does it" is assumption, not truth
- **Ignoring real constraints**: Some things (physics, law, math) actually can't change
- **Analysis paralysis**: First principles is for insight, not infinite regression
- **Throwing away valid conventions**: Sometimes the conventional approach is right
