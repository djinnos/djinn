# Accessibility Audit

Systematically evaluate products against accessibility standards to ensure inclusive design. Accessibility audits catch barriers that exclude users with disabilities.

## When to Use

- Evaluating existing products for compliance
- Before major releases (accessibility gate)
- After design changes to interactive elements
- Responding to accessibility complaints
- Establishing baseline for improvement
- Time: 2-4 hours for comprehensive audit

## Accessibility Standards

### WCAG Levels

| Level | What It Means | When Required |
|-------|---------------|---------------|
| **A** | Minimum accessibility | Basic compliance |
| **AA** | Removes significant barriers | Most regulations require this |
| **AAA** | Highest accessibility | Specialized needs |

**Most organizations target WCAG 2.1 AA.**

### The Four Principles (POUR)

```
┌─────────────────────────────────────────────────────────────┐
│                    WCAG PRINCIPLES                           │
├─────────────────────────────────────────────────────────────┤
│ PERCEIVABLE                                                  │
│ Users must be able to perceive the content                   │
│ • Text alternatives for images                               │
│ • Captions for video                                         │
│ • Sufficient color contrast                                  │
├─────────────────────────────────────────────────────────────┤
│ OPERABLE                                                     │
│ Users must be able to operate the interface                  │
│ • Keyboard accessible                                        │
│ • Enough time to read/use content                            │
│ • No seizure-triggering content                              │
├─────────────────────────────────────────────────────────────┤
│ UNDERSTANDABLE                                               │
│ Users must understand information and interface              │
│ • Readable text                                              │
│ • Predictable behavior                                       │
│ • Input assistance                                           │
├─────────────────────────────────────────────────────────────┤
│ ROBUST                                                       │
│ Content works with current and future technologies           │
│ • Valid HTML                                                 │
│ • Compatible with assistive technologies                     │
└─────────────────────────────────────────────────────────────┘
```

## Audit Process

### Step 1: Scope Definition (10 min)

```markdown
**Accessibility Audit Scope**

- Product/pages: [What's being audited]
- Standard: WCAG [2.0/2.1] Level [A/AA/AAA]
- Audit type: [Automated / Manual / Both]
- Assistive tech tested: [Screen reader, etc.]
- Browsers tested: [List]
```

### Step 2: Automated Testing (30 min)

Run automated tools first to catch obvious issues:

**Tools**:
- axe DevTools (browser extension)
- WAVE (wave.webaim.org)
- Lighthouse (Chrome DevTools)
- Pa11y (command line)

**What Automated Tools Find**:
- Missing alt text
- Color contrast failures
- Missing form labels
- Invalid ARIA
- Heading structure issues

**What They Miss**:
- Meaningful alt text (vs just present)
- Logical tab order
- Screen reader experience
- Cognitive accessibility
- Complex interaction patterns

### Step 3: Manual Testing (2-3 hours)

#### Keyboard Testing
```markdown
**Keyboard Navigation Checklist**

□ All interactive elements reachable with Tab
□ Tab order follows logical reading order
□ Focus indicator visible on all elements
□ No keyboard traps (can Tab in and out)
□ Skip links work (skip to main content)
□ Dropdowns/modals work with keyboard
□ Custom components have keyboard support
□ Shortcuts documented and don't conflict

**Test method**:
Unplug mouse, navigate entire flow with keyboard only.
```

#### Screen Reader Testing
```markdown
**Screen Reader Checklist**

□ Page title announced on load
□ Headings create logical outline
□ Images have meaningful alt text
□ Links have descriptive text (not "click here")
□ Form fields have labels announced
□ Error messages announced
□ Dynamic content changes announced
□ ARIA live regions work correctly

**Test with**:
- VoiceOver (Mac/iOS)
- NVDA (Windows, free)
- JAWS (Windows)
- TalkBack (Android)
```

#### Visual Testing
```markdown
**Visual Accessibility Checklist**

□ Color contrast meets requirements
  - Normal text: 4.5:1 minimum (AA)
  - Large text: 3:1 minimum (AA)
  - UI components: 3:1 minimum
□ Information not conveyed by color alone
□ Text resizable to 200% without loss
□ No text in images (except logos)
□ Focus indicators have sufficient contrast
□ Motion can be paused/stopped

**Tools**:
- Contrast checker: webaim.org/resources/contrastchecker
- Color blindness: Sim Daltonism, Chrome DevTools
```

### Step 4: Document Findings

```markdown
## Accessibility Issue

**ID**: A11Y-[number]
**WCAG Criterion**: [e.g., 1.1.1 Non-text Content]
**Level**: [A / AA / AAA]
**Severity**: [Critical / Major / Minor]

### Description
[What the issue is]

### Location
[Page, component, element]

### Impact
[Who is affected and how]

### Current State
[What happens now]

### Expected State
[What should happen]

### Recommendation
[How to fix]

### Screenshot/Code
[Evidence]
```

## Accessibility Audit Template

```markdown
## Accessibility Audit Report

**Product**: [Name]
**URL(s)**: [List of pages tested]
**Date**: [Audit date]
**Auditor**: [Name]
**Standard**: WCAG 2.1 Level AA

---

### Executive Summary

**Overall compliance**: [Pass with issues / Fail]
**Critical issues**: [Count]
**Major issues**: [Count]
**Minor issues**: [Count]

**Key findings**:
1. [Most significant issue]
2. [Second most significant]
3. [Third most significant]

---

### Testing Methodology

**Automated tools used**:
- [Tool 1] - [Version]
- [Tool 2] - [Version]

**Manual testing**:
- Keyboard navigation: [Yes/No]
- Screen reader: [Which one(s)]
- Browser zoom: [Tested to X%]
- Color contrast: [Yes/No]

**Assistive technologies**:
| Technology | Version | Platform |
|------------|---------|----------|
| VoiceOver | [Ver] | macOS [Ver] |
| NVDA | [Ver] | Windows [Ver] |

---

### Results by WCAG Principle

#### Perceivable

| Criterion | Level | Status | Issues |
|-----------|-------|--------|--------|
| 1.1.1 Non-text Content | A | Pass/Fail | [Count] |
| 1.4.3 Contrast (Minimum) | AA | Pass/Fail | [Count] |
| 1.4.4 Resize Text | AA | Pass/Fail | [Count] |

#### Operable

| Criterion | Level | Status | Issues |
|-----------|-------|--------|--------|
| 2.1.1 Keyboard | A | Pass/Fail | [Count] |
| 2.4.3 Focus Order | A | Pass/Fail | [Count] |
| 2.4.7 Focus Visible | AA | Pass/Fail | [Count] |

#### Understandable

| Criterion | Level | Status | Issues |
|-----------|-------|--------|--------|
| 3.1.1 Language of Page | A | Pass/Fail | [Count] |
| 3.2.1 On Focus | A | Pass/Fail | [Count] |
| 3.3.2 Labels or Instructions | A | Pass/Fail | [Count] |

#### Robust

| Criterion | Level | Status | Issues |
|-----------|-------|--------|--------|
| 4.1.1 Parsing | A | Pass/Fail | [Count] |
| 4.1.2 Name, Role, Value | A | Pass/Fail | [Count] |

---

### Issue Details

[Use the issue template for each finding]

---

### Recommendations

**Immediate (Critical issues)**:
1. [Action item]
2. [Action item]

**Short-term (Major issues)**:
1. [Action item]
2. [Action item]

**Ongoing**:
- [Process improvement]
- [Training need]

---

### Appendix

- Full automated test results
- Screen reader test recordings
- Reference links to WCAG criteria
```

## Quick Accessibility Checklist

For rapid checks when full audit isn't needed:

```markdown
**Quick A11y Check**

Images:
□ All images have alt text
□ Decorative images have empty alt=""
□ Complex images have long descriptions

Color:
□ Text contrast >= 4.5:1
□ Info not conveyed by color alone

Keyboard:
□ All interactive elements focusable
□ Focus order logical
□ Focus visible

Forms:
□ All fields have labels
□ Required fields indicated
□ Errors clearly described

Structure:
□ One h1 per page
□ Headings in logical order
□ Lists use ul/ol/dl elements

Links:
□ Link text describes destination
□ No "click here" or "read more" alone
```

## Tips for Better Accessibility

### Test with Real Users
- Include users with disabilities in usability testing
- Their experience reveals issues tools miss

### Design Accessible from Start
- Easier than retrofitting
- Include in design review criteria

### Prioritize by Impact
- Critical: Prevents access entirely
- Major: Significant barrier
- Minor: Inconvenience

### Document for Developers
- Include code examples
- Reference WCAG success criteria
- Explain the "why" not just the "what"

## Common Accessibility Issues

| Issue | WCAG | Fix |
|-------|------|-----|
| Missing alt text | 1.1.1 | Add descriptive alt |
| Low contrast | 1.4.3 | Increase contrast ratio |
| No focus indicator | 2.4.7 | Add visible focus style |
| Keyboard trap | 2.1.2 | Ensure Esc/Tab exits |
| Missing form labels | 1.3.1 | Associate label with input |
| Non-descriptive links | 2.4.4 | Use meaningful link text |
| Missing page title | 2.4.2 | Add unique, descriptive title |
| Auto-playing media | 1.4.2 | Add pause/stop controls |

## Resources

- WCAG Quick Reference: w3.org/WAI/WCAG21/quickref
- WebAIM: webaim.org
- A11y Project: a11yproject.com
- Inclusive Components: inclusive-components.design
