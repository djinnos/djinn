# Red Team / Blue Team

Adversarial analysis where one side attacks and the other defends. Borrowed from military and security practices, this technique finds vulnerabilities through simulated opposition.

## When to Use

- Testing security of systems or processes
- Validating business strategies against competition
- Stress-testing plans before launch
- Finding holes in arguments or proposals
- Time: 30-60 minutes

## The Roles

### Red Team (Attackers)
**Mission**: Find every way this could fail, be exploited, or break.

**Mindset**:
- "I want this to fail"
- "What's the weakest point?"
- "How would a smart adversary attack this?"
- "What assumptions can I violate?"

**Attack vectors**:
- Technical vulnerabilities
- Process gaps
- Human factors (social engineering, errors)
- Resource constraints
- External threats
- Edge cases and exceptions

### Blue Team (Defenders)
**Mission**: Justify decisions, strengthen weak points, prove resilience.

**Mindset**:
- "Why is this the right approach?"
- "How do we handle that attack?"
- "What safeguards exist?"
- "Where do we need to add protection?"

**Defense strategies**:
- Explain existing mitigations
- Propose new safeguards
- Accept valid vulnerabilities
- Prioritize fixes by severity

## The Process

### Round 1: Red Team Attack (15-20 min)

Red team generates attacks without filtering:

**Attack categories**:
1. **Direct attacks**: How do we break the core functionality?
2. **Indirect attacks**: What adjacent systems can we exploit?
3. **Resource attacks**: Can we overwhelm, exhaust, or starve it?
4. **Social attacks**: How do we manipulate the humans involved?
5. **Timing attacks**: When is it most vulnerable?

**Output**: List of attack vectors with severity assessment

| Attack | Vector | Severity | Likelihood |
|--------|--------|----------|------------|
| [Description] | [How] | H/M/L | H/M/L |

### Round 2: Blue Team Defense (15-20 min)

Blue team responds to each attack:

**For each attack**:
- Acknowledge or dispute the vulnerability
- Explain existing mitigations (if any)
- Propose additional safeguards
- Estimate cost to fix vs. cost of exploitation

**Output**: Defense assessment

| Attack | Current Mitigation | Proposed Fix | Priority |
|--------|-------------------|--------------|----------|
| [Attack] | [Existing defense] | [New safeguard] | P1/P2/P3 |

### Round 3: Escalation (10-15 min)

Red team tries to break the defenses:

- "Your mitigation assumes X - what if X fails?"
- "That safeguard has its own vulnerabilities..."
- "You can't afford to implement that defense..."

Blue team responds:

- Strengthen position with deeper justification
- Accept valid criticisms and escalate to action items
- Identify acceptable risk levels

### Round 4: Synthesis (10 min)

Together, create action plan:

1. **Critical fixes**: Must address before proceeding
2. **Important improvements**: Address within [timeframe]
3. **Accepted risks**: Documented with rationale
4. **Monitoring**: How to detect if attacks occur

## Example Session

**Subject**: New customer data API

**Red Team Attacks**:
1. "Rate limiting is too generous - we can scrape all customer data in hours"
2. "API key in URL parameters means it ends up in logs"
3. "No field-level permissions - all or nothing access"
4. "Error messages reveal internal structure"
5. "No audit logging - breaches go undetected"

**Blue Team Defense**:
1. "Fair point - will reduce rate limits and add anomaly detection"
2. "Acknowledged - will move to header-based authentication"
3. "Intentional for v1 - adding field-level permissions in Q2"
4. "Will implement generic error messages in production"
5. "Audit logging exists but isn't comprehensive - expanding scope"

**Red Team Escalation**:
- "Q2 is too late for field-level permissions - you're launching to enterprise customers in Q1"
- "Rate limiting + anomaly detection still allows slow exfiltration over weeks"

**Final Actions**:
- P0: Fix authentication method before launch
- P0: Accelerate field-level permissions to launch
- P1: Implement comprehensive audit logging
- P2: Add slow-exfiltration detection
- Accepted: Generic error messages can wait for v1.1

## Solo Red/Blue Teaming

When working alone:

1. **Write red attacks first** - Be ruthless, list everything
2. **Step away briefly** - Clear your head
3. **Return as blue team** - Defend genuinely
4. **Alternate rounds** - Keep going until attacks are weak
5. **Synthesize** - Create action list

**Tip**: Written format helps maintain separation between attacker/defender mindsets.

## Tips for Effective Red Teaming

### For Red Team
- **Be creative**: Think like different types of attackers
- **Go meta**: Attack the defense mechanisms themselves
- **Consider insiders**: Not all threats are external
- **Time matters**: Attack during deployment, maintenance, transitions
- **Chain attacks**: Small vulnerabilities combine into big ones

### For Blue Team
- **Don't be defensive** (emotionally): Valid attacks help you
- **Admit unknowns**: "We don't have mitigation for that yet"
- **Quantify risk**: Not all vulnerabilities need fixing
- **Propose, don't promise**: Solutions need validation too

## Common Mistakes

- **Pulling punches**: Red team goes easy to avoid conflict
- **Taking it personally**: Blue team gets defensive about their work
- **Stopping at obvious attacks**: Real adversaries are creative
- **Defending everything**: Some attacks aren't worth mitigating
- **No follow-through**: Findings collected but never acted on
