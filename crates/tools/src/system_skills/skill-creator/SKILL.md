---
name: skill-creator
description: Create a new skill, or improve one, together with the person. Use when they ask to make, write or improve a skill, or to turn a procedure they keep repeating into one.
required_tools: [skill.save]
---

# Skill creator

A skill is a procedure Medha loads when a task needs it: a name, a one-line description, and the steps. Only the
description sits in every chat; the steps load when the skill is used. So the description decides whether the skill
is ever picked, and the steps decide whether it helps.

## 1. Understand the job

Ask, in one message, only what is still missing:
- What should it do, with which inputs, and what should it produce?
- When should it be used? Two or three real requests that should bring it in, and one that should not.
- Is there a procedure, document or example to start from? If so, read it first.

## 2. Name and describe

- Name: short kebab-case, the job or the thing (`release-notes`, `pdf-forms`).
- Description: what it does and when to use it, in the person's own words, under 300 characters. Use the words they
  will actually say when they need it. No praise, no filler.

## 3. Write the steps

- Numbered steps in the order they run, each one an action and the tool it uses.
- The checks to make at each decision, and what to do on each answer.
- What tends to go wrong, and how to recover.
- Only what a capable model would not do on its own; leave general advice out.
- Under about 150 lines. Name the tools it needs in `required_tools`.

## 4. Show it, then save it

Show the person the name, the description and the steps. When they agree, save it with `skill.save` and
`job: "write"`; they approve the card. It goes to their skills for every project, unless they asked for this project
(`scope: "project"`). To improve a skill, load it with `skill`, then save the new version under the same name: the
card shows what changes.

## 5. Try it

Offer to run one of the example requests with the new skill, and adjust the steps from what happens.
