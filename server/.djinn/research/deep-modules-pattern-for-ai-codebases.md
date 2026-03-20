---
title: Deep Modules Pattern for AI Codebases
type: research
tags: ["architecture","ai-friendly","deep-modules","ousterhout"]
---

## Source
- Book: *A Philosophy of Software Design* — John Ousterhout (2018)
- Video: [Matt Pocock — Your codebase is NOT ready for AI](https://www.youtube.com/watch?v=uC44zFz7JSM)
- Blog: [Nagarro — Deep-Module, Low-Complexity Future](https://www.nagarro.com/en/blog/deep-module-low-complexity-software-design)
- Blog: [Paul Simmering — A Philosophy of AI Coding](https://simmering.dev/blog/a-philosophy-of-ai-coding/)
- Harvested: 2026-03-12

## Definition

A module is **deep** when its implementation is significantly more complex than its interface — a simple interface hiding a complex implementation. All exports go through the interface; nothing leaks.

## Why Deep Modules Matter for AI

- Simple interfaces minimize what the agent must hold in context to use a module correctly
- Complex implementations stay hidden — the agent does not need to traverse them to compose with the module
- Changes are isolated to implementations without rippling through consumers
- Agents can form correct mental models from interface alone
- Reduces context surface area from hundreds of inter-related files to 7-8 core interface chunks

### The "Graybox Module" Extension (Pocock)

Deep modules become "graybox modules" when combined with tests:
- You don't need to look inside the module if the tests pass
- AI manages the internals; humans design the interfaces
- Tests lock down behavioral contracts at the boundary
- This is NOT vibe coding — taste is applied at the boundaries

### Progressive Disclosure of Complexity

The interface sits at the top and explains what the module does. When needed, look inside to understand behavior more deeply. AI reads the interface first, only dives into implementation when required.

## Anti-Pattern: Shallow Modules

Many small functions/classes with trivial bodies but large surface area. Forces agents to traverse more code to accomplish less. The "micro-module trap" — each module is testable in tiny units but really hard to keep all in your head (or context window).

## Quantitative Impact

- Clean module, explicit interface: ~95% AI accuracy
- Moderate coupling, implicit deps: ~80%
- Tight coupling: ~60%
- Structural improvements yield **35% accuracy gains** vs. 10-20% from model upgrades

## Key Quote

> "You're spawning 20 new starters every day. Your codebase needs to be friendly to new starters." — Matt Pocock

## Implications for Verification

Deep modules with clear interfaces enable **targeted verification**: run tests only for the affected module + its direct importers, not the whole app. The interface boundary defines the blast radius.

## Relations
- [[Vertical Slice Architecture for AI]]
- [[Hexagonal Architecture as AI Prompt]]
- [[Pit of Success Pattern for AI]]
- [[Type Systems as AI Architecture Guidance]]
