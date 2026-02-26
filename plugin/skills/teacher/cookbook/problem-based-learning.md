# Problem-Based Learning Methodology

Learn by solving authentic, relevant problems. Knowledge emerges from need - you learn because you have to in order to solve something that matters.

## When to Use

- User needs to learn for practical application
- Motivation comes from relevance
- Building problem-solving skills alongside knowledge
- User has a real task they're working toward
- Time available: 30-45 minutes

## The Five Phases

### Phase 1: Present Authentic Problem

Start with a problem that creates need-to-know.

**Good problem characteristics**:
- Relevant to user's actual work or interests
- Complex enough to require new learning
- Open-ended (multiple valid approaches)
- Authentic (real-world, not contrived)
- Clear enough to engage with, vague enough to require investigation

**Sourcing problems**:
1. **Best**: Use user's actual challenge ("What are you trying to build/solve?")
2. **Good**: Create relevant scenario based on their domain
3. **Acceptable**: Present canonical problem in their interest area

**Example framing**:
> "Here's a situation: [scenario]. Don't worry about having all the knowledge yet - that's the point. How would you start approaching this?"

### Phase 2: Analyze Problem and Identify Gaps

Help user break down what they know vs. need to know.

**Analysis questions**:
- "What aspects of this problem do you already understand?"
- "What would you need to know to make progress?"
- "What skills or concepts are you missing?"
- "What's the hardest part of this problem?"

**Create a learning map**:

| Already Know | Need to Know | How to Learn |
|--------------|--------------|--------------|
| [user's existing knowledge] | [identified gaps] | [resources/approach] |

**Prioritize gaps**: Focus on what's most blocking progress, not everything at once.

### Phase 3: Research and Learn

Targeted learning to fill the most critical gaps.

**Learning options**:
- User researches independently (most autonomy)
- Guided explanation of specific concepts (when stuck)
- Pointers to resources (balanced approach)
- Mini-lessons on key topics (when efficiency matters)

**Stay problem-focused**:
- "How does this concept help with our problem?"
- "What part of the problem can you solve now?"
- "What's still missing?"

**Avoid**: Learning everything about a topic. Only what's needed for progress.

### Phase 4: Apply Knowledge

Develop solution using new knowledge.

**Application prompts**:
- "Now that you understand [X], how does that change your approach?"
- "Try solving the first part of the problem"
- "Walk me through your thinking as you apply this"
- "What happens when you try [approach]?"

**Guide without solving**:
- Point out when approach might hit issues
- Ask about edge cases they haven't considered
- Connect back to learning when stuck

**Iteration is expected**: First attempts rarely work. Each failure is data.

### Phase 5: Reflect on Learning

Consolidate learning and enable transfer.

**Reflection prompts**:
- "What did you learn that you didn't expect?"
- "What was the hardest part?"
- "How would you approach a similar problem next time?"
- "What would you do differently?"
- "Where else might this knowledge apply?"

**Extract principles**: Help user move from "I solved this specific problem" to "I understand this class of problems."

## Problem Design Tips

### Scaling Complexity

| Level | Problem Characteristics |
|-------|------------------------|
| Beginner | Single concept needed, clear path, scaffolded |
| Intermediate | 2-3 concepts, multiple approaches, some ambiguity |
| Advanced | Cross-domain, significant unknowns, real constraints |

### Building Good Problems

1. **Start with the learning goal** - What should they understand after?
2. **Work backward to scenario** - What situation requires that knowledge?
3. **Add authentic constraints** - Real-world limitations make it realistic
4. **Remove artificial hints** - Don't telegraph the solution
5. **Test the gap** - Is learning actually required, or can they solve it already?

## Example Session

**User**: "I want to learn about database indexing"

**Teacher**: "Let's make it concrete. Imagine you've built an app with a users table that has 10 million rows. Users are searching by email to log in, and it's taking 3-4 seconds. How would you investigate and fix this?"

**User**: "I'd... look at the query? Maybe the database is slow?"

**Teacher**: "Good starting point. What do you know about how databases find rows?"

**User**: "I think it goes through the table looking for matches?"

**Teacher**: "Right - that's called a table scan. With 10 million rows, what's the problem with that approach?"

**User**: "It has to check every single row, even if the email is in the first 100."

**Teacher**: "Exactly. So what might 'indexing' do to help?"

**User**: "Maybe... organize the data so it doesn't have to check everything? Like how a book index tells you what page to go to?"

**Teacher**: "Good analogy. So if you created an index on the email column, what would the database be able to do differently?"

**User**: "Jump straight to the right rows instead of scanning everything?"

**Teacher**: "Now you're getting it. Want to figure out how to actually create an index and test if it helps?"

[Continue to application and reflection phases]

## Common Pitfalls

- **Fake problems** - Contrived scenarios with obvious "teaching" intent
- **Over-scoping** - Problems requiring too much learning at once
- **Solving for them** - Providing answers instead of guiding discovery
- **Skipping reflection** - Missing the consolidation that enables transfer

## The Meta-Skill

Problem-based learning teaches problem-solving itself. The pattern of "identify gaps → learn what's needed → apply → reflect" transfers to any domain. Users who internalize this approach become self-directed learners.
