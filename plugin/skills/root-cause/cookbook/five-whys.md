# Five Whys

Reach root cause by repeatedly asking "why?" Originally from Toyota's manufacturing process, this technique cuts through symptoms to fundamental causes.

## When to Use

- Understanding why a problem occurs
- Problem keeps recurring despite fixes
- Want to avoid band-aid solutions
- Need to explain a situation clearly
- Time: 10-20 minutes

## The Process

### Step 1: State the Problem Clearly

Define what's happening in specific, observable terms.

**Bad problem statements**:
- "Performance is bad" (vague)
- "Users are unhappy" (subjective)
- "The system doesn't work" (undefined)

**Good problem statements**:
- "Page load time exceeds 5 seconds for 30% of users"
- "Support tickets doubled in the last month"
- "Deployment failed 3 times this week"

### Step 2: Ask "Why?" Repeatedly

For each answer, ask why again. Continue until you reach a cause you can act on.

**Format**:
```
Problem: [Observable problem]
Why 1: [First-level cause] → Why?
Why 2: [Deeper cause] → Why?
Why 3: [Deeper still] → Why?
Why 4: [Getting fundamental] → Why?
Why 5: [Root cause - something you can address]
```

**Tips for asking why**:
- Ask with genuine curiosity, not accusation
- Accept "I don't know" as a signal to investigate
- Follow the chain of causation, not blame
- Sometimes you need more than 5, sometimes fewer

### Step 3: Verify the Root Cause

Test whether you've found the actual root:

**Root cause tests**:
- If we fix this, does the original problem go away?
- Can we go deeper? (If yes, keep going)
- Is this actionable? (Root causes should be fixable)
- Does this explain other related problems?

### Step 4: Define Action

Once at root cause, define specific actions:

- What needs to change?
- Who owns making it happen?
- How will we know it worked?
- How do we prevent recurrence?

## Example: The Classic

**Problem**: The Jefferson Memorial is deteriorating faster than other monuments.

- Why? Because it gets washed more frequently.
- Why? Because it has more bird droppings.
- Why? Because there are more birds around it.
- Why? Because there are more spiders for birds to eat.
- Why? Because there are more midges (spider food) around the memorial.
- Why? Because the lights are turned on at dusk, attracting midges.

**Root cause**: Timing of lights
**Solution**: Turn on lights later
**Result**: Midge population dropped, as did birds, spiders, droppings, and washing frequency.

## Example: Software Development

**Problem**: Users abandoning signup flow

- Why are users abandoning? Form takes too long to complete.
- Why does it take too long? We ask for 15 fields of information.
- Why do we ask for so much? We thought we needed it for personalization.
- Why do we think we need it all upfront? We never tested progressive profiling.
- Why haven't we tested it? We assumed signup had to be complete before access.

**Root cause**: Untested assumption about when to collect data
**Solution**: Test progressive profiling - collect basics at signup, rest during usage
**Result**: Signup completion increased 40%

## Branching Five Whys

Sometimes causes branch. Follow each branch.

```
Problem: Deployment failed
├── Why? Tests failed
│   ├── Why? Test environment differs from production
│   │   └── Why? Environment setup is manual
│   │       └── Root: No infrastructure-as-code
│   └── Why? New test was flaky
│       └── Why? Test depends on timing
│           └── Root: Missing async handling
└── Why? Deployment script error
    └── Why? Script wasn't updated for new service
        └── Root: No deployment checklist
```

**Multiple root causes**: Often you'll find several. Address the most impactful first.

## Five Whys Workshop Format

**For teams (30 min)**:

1. **Define problem** (5 min) - Get specific, observable problem statement
2. **Individual why chains** (10 min) - Each person writes their own chain
3. **Share and discuss** (10 min) - Compare chains, identify common roots
4. **Action planning** (5 min) - Define fixes for top root causes

## Tips for Effective Five Whys

### Keep asking
The first "why" is almost never the root cause. If you stop early, you'll treat symptoms.

### Watch for blame
"Why? Because John made a mistake" isn't root cause - ask why the mistake was possible.

### Look for systems
Root causes are usually systemic (process, tooling, communication) not individual.

### Accept multiple causes
Real problems rarely have single causes. Branch your analysis.

### Verify, don't assume
"I think this is the root cause" should lead to investigation, not immediate action.

## Common Mistakes

- **Stopping too early**: Surface causes feel like root causes
- **Accepting blame as cause**: "Human error" is a symptom, not a root
- **Single chain thinking**: Missing that multiple factors contribute
- **No verification**: Assuming root cause without testing
- **No action**: Finding root cause but not fixing it
- **Asking "who" instead of "why"**: This is about systems, not people

## When Five Whys Isn't Enough

Five Whys works best for linear cause-and-effect. For complex systems with multiple interacting factors, consider:
- Fishbone diagrams (Ishikawa)
- Fault tree analysis
- Systems thinking approaches

Five Whys is a starting point, not always the complete answer.
