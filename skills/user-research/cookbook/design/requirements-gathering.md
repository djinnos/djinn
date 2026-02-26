# Requirements Gathering

Systematically capture constraints, requirements, and trade-offs that shape design decisions. Good requirements gathering prevents rework and misaligned expectations.

## When to Use

- Starting a new project or major feature
- Defining technical specifications
- Establishing accessibility requirements
- Clarifying stakeholder expectations
- Documenting constraints before design
- Time: 20-30 minutes per stakeholder/area

## Types of Requirements

| Type | What It Covers | Example |
|------|----------------|---------|
| **Functional** | What the system must do | "Users can filter search results" |
| **Non-functional** | How the system must perform | "Page loads in < 2 seconds" |
| **Technical** | Technology constraints | "Must work in IE11" |
| **Business** | Organizational constraints | "Launch by Q2" |
| **User** | User needs and limitations | "Must be screen-reader accessible" |

## The Requirements Framework

```
┌─────────────────────────────────────────────────────────────┐
│               REQUIREMENTS GATHERING                         │
├─────────────────────────────────────────────────────────────┤
│ TECHNICAL CONSTRAINTS                                        │
│ • Browser/device support                                     │
│ • Performance requirements                                   │
│ • Integration requirements                                   │
│ • Security requirements                                      │
├─────────────────────────────────────────────────────────────┤
│ BUSINESS CONSTRAINTS                                         │
│ • Timeline limitations                                       │
│ • Budget constraints                                         │
│ • Brand guidelines                                           │
│ • Legal/compliance needs                                     │
├─────────────────────────────────────────────────────────────┤
│ USER CONSTRAINTS                                             │
│ • Accessibility requirements                                 │
│ • Device/platform usage                                      │
│ • Bandwidth limitations                                      │
│ • Technical literacy levels                                  │
├─────────────────────────────────────────────────────────────┤
│ TRADE-OFFS                                                   │
│ • What if we had to choose between A and B?                  │
│ • What's the minimum viable experience?                      │
│ • Where can we compromise?                                   │
└─────────────────────────────────────────────────────────────┘
```

## The Process

### Step 1: Identify Stakeholders (10 min)

Who has input on requirements?

| Stakeholder | What They Know |
|-------------|----------------|
| Product | Business goals, priorities, timelines |
| Engineering | Technical constraints, feasibility |
| Design | User needs, UX requirements |
| Legal/Compliance | Regulatory requirements |
| Security | Security and privacy requirements |
| Operations | Support, maintenance needs |

### Step 2: Gather by Category (20 min each)

**Technical Requirements Questions**:
```markdown
- What browsers/devices must be supported?
- What are the performance requirements (load time, etc.)?
- What existing systems must this integrate with?
- What security/privacy requirements apply?
- What APIs or data sources are involved?
- Are there infrastructure constraints?
```

**Business Requirements Questions**:
```markdown
- What is the timeline? Any hard deadlines?
- What is the budget constraint?
- What brand guidelines must be followed?
- Are there legal or compliance requirements?
- Who needs to approve the final result?
- What are the success metrics?
```

**User Requirements Questions**:
```markdown
- What accessibility level is required (WCAG A/AA/AAA)?
- What devices do users primarily use?
- What is users' technical proficiency?
- Are there connectivity/bandwidth considerations?
- What languages/localization is needed?
- Are there assistive technology considerations?
```

### Step 3: Explore Trade-offs (15 min)

Force prioritization discussions:

```markdown
Trade-off Questions:
- "If we had to choose between [speed] and [features], which wins?"
- "What's the absolute minimum that would be acceptable?"
- "If timeline shrinks by 50%, what gets cut?"
- "Would you sacrifice [A] to ensure [B]?"
- "What's non-negotiable vs nice-to-have?"
```

### Step 4: Document and Validate (10 min)

Capture in structured format and get confirmation.

## Requirements Document Template

```markdown
## Requirements Document

**Project**: [Name]
**Date**: [Date]
**Version**: [Version]
**Status**: Draft / Under Review / Approved

### Overview
[Brief description of what's being built and why]

---

### Technical Requirements

#### Browser/Device Support
| Platform | Version | Support Level |
|----------|---------|---------------|
| Chrome | Latest 2 | Full |
| Safari | Latest 2 | Full |
| Firefox | Latest 2 | Full |
| Edge | Latest 2 | Full |
| iOS Safari | 14+ | Full |
| Android Chrome | Latest | Full |

#### Performance
| Metric | Requirement |
|--------|-------------|
| Initial load | < 3 seconds on 3G |
| Time to interactive | < 5 seconds |
| Largest Contentful Paint | < 2.5 seconds |
| Core Web Vitals | Pass |

#### Security
- [ ] HTTPS required
- [ ] Authentication: [Method]
- [ ] Data encryption: [At rest / In transit]
- [ ] PII handling: [Requirements]

#### Integrations
| System | Type | Priority |
|--------|------|----------|
| [System] | [API/SSO/etc] | [Must/Should/Could] |

---

### Business Requirements

#### Timeline
- Start date: [Date]
- Target launch: [Date]
- Hard deadline: [Date or N/A]

#### Budget
- Development: [Amount or TBD]
- Design: [Amount or TBD]

#### Approvals Required
| Stage | Approver |
|-------|----------|
| Design | [Name] |
| Development | [Name] |
| Launch | [Name] |

#### Legal/Compliance
- [ ] GDPR compliance required
- [ ] CCPA compliance required
- [ ] [Industry-specific]: [Details]

---

### User Requirements

#### Accessibility
- WCAG Level: [A / AA / AAA]
- Screen reader support: [Required / Nice-to-have]
- Keyboard navigation: [Full / Partial]
- Color contrast: [Ratio requirement]

#### Localization
| Language | Priority |
|----------|----------|
| English | Required |
| [Other] | [Priority] |

#### User Context
- Primary device: [Desktop / Mobile / Both]
- Technical proficiency: [Low / Medium / High]
- Typical connection: [Broadband / Mobile / Limited]

---

### Trade-offs & Priorities

#### MoSCoW Prioritization
| Category | Items |
|----------|-------|
| **Must Have** | [List non-negotiables] |
| **Should Have** | [List important but not critical] |
| **Could Have** | [List nice-to-haves] |
| **Won't Have** | [List explicitly out of scope] |

#### Documented Trade-offs
| Trade-off | Decision | Rationale |
|-----------|----------|-----------|
| Speed vs Features | [Choice] | [Why] |
| [Other] | [Choice] | [Why] |

---

### Open Questions
- [ ] [Question needing resolution]
- [ ] [Question needing resolution]

### Sign-off
| Role | Name | Date | Signature |
|------|------|------|-----------|
| Product | | | |
| Engineering | | | |
| Design | | | |
```

## Tips for Better Requirements Gathering

### Ask "Why" Behind Requirements
- "Why is IE11 support needed?" → Maybe it's not
- "Why 2-second load time?" → Understand the actual user impact

### Separate Requirements from Solutions
- Requirement: "Users need to compare options"
- Not a requirement: "A comparison table" (that's a solution)

### Get Specific Numbers
- Not: "Must be fast"
- Better: "Must load in < 3 seconds on 3G connection"

### Document Assumptions
- What are you assuming is true?
- What happens if those assumptions are wrong?

### Plan for Change
- Requirements will evolve
- Version and date your documents
- Note what changed and why

## Common Mistakes

- **Vague requirements**: "Must be user-friendly" (meaningless)
- **Solutions disguised as requirements**: "Must use React" (why?)
- **Missing stakeholders**: Not consulting security, legal, etc.
- **No prioritization**: Everything is "must have"
- **Outdated docs**: Requirements doc doesn't match current state
- **Assumed requirements**: "Everyone knows we need X"
- **Gold plating**: Requirements beyond actual need
