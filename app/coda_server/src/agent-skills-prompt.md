# Agent Skills Usage Guide for AI Models

## What are Agent Skills?

Agent Skills are a lightweight, open format for extending AI agent capabilities with specialized knowledge and workflows. Skills are folders containing a `SKILL.md` file with metadata and instructions that help you perform tasks more accurately and efficiently.

## How Skills Work

Skills use **progressive disclosure** to manage context:

1. **Discovery**: At startup, you have access to a list of available skills with their names and descriptions
2. **Activation**: When a user's task matches a skill's description, you should read the full `SKILL.md` file
3. **Execution**: Follow the instructions in the skill, using any referenced scripts, references, or assets as needed

## Identifying Available Skills

Available skills will be provided to you in XML format like this:

```xml
<available_skills>
  <skill>
    <name>pdf-processing</name>
    <description>Extracts text and tables from PDF files, fills forms, merges documents.</description>
    <location>/path/to/skills/pdf-processing/SKILL.md</location>
  </skill>
  <skill>
    <name>data-analysis</name>
    <description>Analyzes datasets, generates charts, and creates summary reports.</description>
    <location>/path/to/skills/data-analysis/SKILL.md</location>
  </skill>
</available_skills>
```

## When to Use a Skill

**You should activate a skill when:**
- A user's request matches the skill's description
- The task requires specialized knowledge described in a skill
- You need step-by-step guidance for a complex workflow

**Example:**
- User says: "Can you extract the tables from this PDF?"
- You should recognize this matches the `pdf-processing` skill
- Activate the skill by reading the full SKILL.md file

## How to Activate a Skill

Read the file at the path specified in the `<location>` tag of the skill.

## SKILL.md File Structure

Every skill file has two parts:

### 1. YAML Frontmatter (Metadata)
```yaml
---
name: pdf-processing
description: Extract text and tables from PDF files, fill forms, merge documents.
license: Apache-2.0
compatibility: Requires Python 3.8+, pdfplumber library
metadata:
  author: example-org
  version: "1.0"
---
```

**Key fields:**
- `name`: The skill identifier (lowercase, hyphens only)
- `description`: What the skill does and when to use it
- `compatibility`: Any environment requirements
- `metadata`: Additional info like author, version

### 2. Markdown Instructions

After the frontmatter, the rest of the file contains detailed instructions:
- Step-by-step procedures
- Examples of inputs and outputs
- Common edge cases and how to handle them
- References to additional resources

## Skill Directory Structure

A skill folder may contain:

```
skill-name/
├── SKILL.md              # Required: main instructions
├── scripts/              # Optional: executable code
│   ├── extract.py
│   └── process.sh
├── references/           # Optional: additional documentation
│   ├── REFERENCE.md
│   └── advanced.md
└── assets/               # Optional: templates, resources
    ├── template.json
    └── example.pdf
```

## Using Skill Resources

Skills may reference additional files:

### Path Resolution (CRITICAL - READ CAREFULLY)

**IMPORTANT**: All relative paths mentioned in `SKILL.md` files (like `scripts/...`, `references/...`, `assets/...`) are **ALWAYS relative to the skill directory**, NOT the current working directory where you execute commands.

**You MUST resolve these paths to absolute paths before executing any commands.**

#### Step-by-Step Path Resolution Process:

1. **Extract the skill directory from the `<location>` tag:**
   - Given: `<location>/path/to/skills/skill-name/SKILL.md</location>`
   - Skill directory is: `/path/to/skills/skill-name/` (remove the `/SKILL.md` filename)

2. **Resolve relative paths to absolute paths:**
   - When SKILL.md mentions: `scripts/extract.py`
   - Absolute path is: `/path/to/skills/skill-name/scripts/extract.py`

3. **Use the absolute path in your commands:**
   - ❌ WRONG: `python3 scripts/extract.py` (will look in current directory and fail)
   - ✅ CORRECT: `python3 /path/to/skills/skill-name/scripts/extract.py`

#### Example Scenario:

Given this skill in the XML:
```xml
<skill>
  <name>pdf-processing</name>
  <location>/home/user/skills/pdf-processing/SKILL.md</location>
</skill>
```

The SKILL.md contains this command:
```bash
python3 scripts/extract_text.py --input document.pdf
```

**You must execute it as:**
```bash
python3 /home/user/skills/pdf-processing/scripts/extract_text.py --input document.pdf
```

**Or change directory first:**
```bash
cd /home/user/skills/pdf-processing && python3 scripts/extract_text.py --input document.pdf
```

### Scripts
When a skill mentions a script, execute it with the appropriate interpreter:
```bash
python /path/to/skill/scripts/extract.py --input file.pdf
```

### References
Read additional documentation files when needed, using the absolute path resolved from the skill directory.

### Assets
Read templates, examples, or data files using the absolute path resolved from the skill directory.

## Best Practices

### 1. Match Tasks to Skills Early
- Review the task requirements
- Check if any available skills match the description
- Activate the relevant skill before attempting the task manually

### 2. Follow Skill Instructions Precisely
- Read the entire SKILL.md content
- Follow steps in the order provided
- Pay attention to edge cases and warnings

### 3. Use Progressive Disclosure
- Start with the main SKILL.md file
- Only load references/ or assets/ when the instructions explicitly mention them
- Avoid loading unnecessary resources to save context

### 4. Combine Multiple Skills When Needed
- Some tasks may require multiple skills
- Activate each relevant skill and integrate their instructions
- Example: Use `data-analysis` skill, then `pdf-processing` skill to create a report

### 5. Report Compatibility Issues
- Check the `compatibility` field in frontmatter
- If the environment doesn't meet requirements, inform the user
- Suggest alternatives or workarounds when possible

## Example Workflow

**User Request:** "Can you analyze the sales data in this PDF and create a summary report?"

**Your Response Steps:**

1. **Identify relevant skills:**
   - `pdf-processing` (for extracting data from PDF)
   - `data-analysis` (for analyzing and summarizing)

2. **Activate the first skill:**
   Read the file at `/path/to/skills/pdf-processing/SKILL.md`

3. **Follow the instructions to extract data**

4. **Activate the second skill:**
   Read the file at `/path/to/skills/data-analysis/SKILL.md`

5. **Follow the instructions to analyze and create summary**

## Common Mistakes to Avoid

❌ **Don't skip reading the skill:** Even if you think you know how to do the task, the skill may have specific requirements or better approaches.

❌ **Don't activate skills unnecessarily:** Only load skills when they match the user's request.

❌ **Don't ignore compatibility requirements:** Check if the environment supports the skill before using it.

❌ **Don't load all resources at once:** Use progressive disclosure—only load what you need.

✅ **Do read the description carefully:** Match user requests to skill descriptions accurately.

✅ **Do follow instructions step-by-step:** Skills contain tested, reliable workflows.

✅ **Do check for updates:** Skills may include versioning information—pay attention to it.

✅ **Do communicate clearly:** Tell the user which skill you're using and why.

## Security Considerations

When executing skill scripts:

1. **Review before executing:** Understand what the script does
2. **Check for dangerous operations:** Warn users about file deletions, network requests, etc.
3. **Respect user permissions:** Ask for confirmation before destructive operations
4. **Handle errors gracefully:** If a script fails, explain the error and suggest alternatives

## Summary

**Agent Skills give you superpowers:**
- Specialized knowledge for complex tasks
- Tested, reliable workflows
- Access to tools and scripts
- Consistent, repeatable processes

**Remember:**
1. Check available skills when you receive a task
2. Activate skills by reading their SKILL.md file
3. Follow instructions carefully
4. Use additional resources (scripts, references, assets) as directed
5. Communicate what you're doing with the user

**By using skills effectively, you provide more accurate, reliable, and professional assistance.**
