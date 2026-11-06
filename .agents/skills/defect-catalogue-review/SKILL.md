---
name: defect-catalogue-review
description: Use in the final review before a PR merges, and in the version-bump review (all changes in a semantic-version range), to check a diff or a commit range against the defect catalogue of defect classes that we found and fixed before. Read-only; it does not write to the catalogue.
---

# Check a diff against the defect catalogue

The catalogue is in `docs/99-reference/defect-catalogue/`. It lists defect classes that we found and
fixed before. This skill finds new instances of those classes in a diff, at low token cost: it
reads one short detection list, and opens a class file only to confirm a lead.

Do these steps in order.

1. **Scan.** Run the model-free scan for the target:

   ```sh
   cargo xtask defects scan --base <base> --head <head>   # a PR or a version range
   cargo xtask defects scan                               # this branch, working tree vs. merge-base with master
   ```

   Each hit is a lead, not a finding.

2. **Read the detection list.** Read `docs/99-reference/defect-catalogue/detect.md` and nothing
   else from the catalogue yet. Check the diff against each line whose scope it touches: a change
   that matches a "Flag" is a lead. A scan pattern does not find all instances, so read the diff
   for the flags too. Drop leads that match a "Do not flag" line.

3. **Verify each lead.** For each scan hit and each lead, read the code at that site. Then open
   `classes/<id>.md` for that lead's class only, and confirm the lead against its **Pattern** and
   **Why it is a defect**. Do not open the class files of classes with no lead.

4. **Question the tests and the design.** Tests can encode bad behaviour. Do not accept a test as
   proof that the code is correct. Question the design as it is, and what it could be.

5. **Report.** Report only true instances, each with `file:line`, the class id, why it is an
   instance, and the **Correct form** from the class file. For each, suggest an `instances.toml`
   row (to add after the fix ships). For a defect that fits no class, say "candidate new class"
   and give a one-line pattern.

This skill does not change the catalogue. To add classes or instances, use the
`defect-catalogue-add` skill.
