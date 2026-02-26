# Card Sorting

Understand how users mentally organize content and expect to find information. Card sorting reveals natural mental models for information architecture.

## When to Use

- Designing site/app navigation structure
- Organizing feature sets or content areas
- Validating existing information architecture
- Creating menu labels and categories
- Planning content migration or reorganization
- Time: 20-40 minutes per session

## Types of Card Sorting

| Type | When to Use | Process |
|------|-------------|---------|
| **Open** | Exploring mental models | Users create their own category names |
| **Closed** | Validating proposed structure | Users sort into predefined categories |
| **Hybrid** | Balanced approach | Predefined categories + user can create new |

## The Process

### Step 1: Prepare Cards (30 min)

Create cards for each content item or feature:

```
Card Template:
┌─────────────────────┐
│ [Item Name]         │
│                     │
│ [Brief description] │
│ (optional)          │
└─────────────────────┘
```

**Guidelines**:
- 30-60 cards is ideal (too few = trivial, too many = overwhelming)
- Use consistent naming (nouns or verb phrases)
- Include description if item name is ambiguous
- Don't include obvious categories in names

### Step 2: Run the Session (20-40 min)

**Open Sort Instructions**:
```
"Here are [X] cards representing [content/features].
Please group these in a way that makes sense to you.
There's no right or wrong answer - I want to understand how you think about this.

When you're done grouping:
1. Give each group a name that describes it
2. Tell me why you grouped them this way"
```

**Closed Sort Instructions**:
```
"Here are [X] cards and [Y] categories.
Please place each card in the category where you'd expect to find it.
If a card doesn't fit anywhere, put it in 'Unsure'."
```

### Step 3: Probe and Document (10 min)

**Follow-up Questions**:
- "Why did you put [card] in this group?"
- "Where would you look for [specific item]?"
- "What would you call this group?"
- "Were any cards difficult to place? Why?"
- "What's missing from these categories?"

### Step 4: Analyze Results

**For multiple participants**, look for:

| Metric | What It Tells You |
|--------|-------------------|
| Agreement rate | How consistently cards are grouped together |
| Popular categories | Natural groupings that emerge |
| Problem cards | Items that don't fit cleanly anywhere |
| Naming patterns | Labels users naturally use |

## Card Sorting Template

```markdown
## Card Sorting Session

**Project**: [Name]
**Date**: [Date]
**Participant**: [ID or role]
**Sort Type**: Open / Closed / Hybrid

### Cards Used
1. [Card 1]
2. [Card 2]
...

### Categories (Closed sort only)
- [Category A]
- [Category B]
...

### Results

**Group 1**: [User's name for group]
- [Card]
- [Card]
- [Card]

**Group 2**: [User's name for group]
- [Card]
- [Card]

**Unsure / Didn't fit**:
- [Card] - "[User's comment]"

### Key Insights
- [Insight 1]
- [Insight 2]

### Implications for IA
- [Recommendation]
```

## Virtual Card Sorting

When in-person isn't possible:

**Tools**: OptimalSort, UserZoom, Miro, FigJam

**Virtual Session Protocol**:
```
1. Share screen with card sorting tool
2. Explain the task clearly
3. Think-aloud: "Tell me what you're thinking as you sort"
4. Record the session for later analysis
5. Take notes on hesitations and comments
```

## Analysis Methods

### Similarity Matrix
Shows how often cards were grouped together across participants.

```
Cards grouped together:
         Card A   Card B   Card C   Card D
Card A     -       80%      20%      10%
Card B    80%       -       60%      15%
Card C    20%      60%       -       90%
Card D    10%      15%      90%       -
```

**High agreement (>70%)**: Strong case for grouping together
**Low agreement (<30%)**: Cards may need different home or clearer labeling

### Dendrogram
Hierarchical clustering showing natural groupings at different levels.

## Tips for Better Card Sorting

### Do
- Run with 15-30 participants for statistical validity
- Include diverse user types
- Test your cards with 1-2 people first
- Record think-aloud during sorting
- Look for patterns, not just majority votes

### Don't
- Include too many cards (max 60)
- Use jargon users won't understand
- Lead participants toward expected groupings
- Ignore outliers - they reveal edge cases
- Skip follow-up questions

## Common Mistakes

- **Too specific**: Cards like "FAQ Question 47" instead of "FAQ"
- **Too general**: Cards like "Content" that could go anywhere
- **Missing context**: No description when card name is ambiguous
- **Small sample**: Drawing conclusions from 3-5 participants
- **Ignoring conflicts**: Different user types may need different IA
- **Literal translation**: Copying user labels directly without interpretation
