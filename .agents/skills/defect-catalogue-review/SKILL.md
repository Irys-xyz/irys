---
name: defect-catalogue-review
description: Use in the final review before a PR merges, and in the version-bump review (all changes in a semantic-version range), to check a diff or a commit range against the defect catalogue of defect classes that we found and fixed before. Read-only; it does not write to the catalogue.
---

# Check a diff against the defect catalogue

The catalogue is in `docs/99-reference/defect-catalogue/`. It lists defect classes that we found and
fixed before. This skill finds new instances of those classes in a diff, at low token cost.

Do these steps in order.

1. **Scan.** Run the model-free scan for the target:

   ```sh
   cargo xtask defects scan --base <base> --head <head>   # a PR or a version range
   cargo xtask defects scan                               # this branch, working tree vs. merge-base with master
   ```

   Each hit is a lead, not a finding.

2. **Read the index only.** Read the index table in `docs/99-reference/defect-catalogue/README.md`.
   Select the classes whose "Applies when" matches the diff, plus every class that has a scan hit.
   Do not read the other class files.

3. **Read the selected classes.** Open only `classes/<id>.md` for each selected class. Check the
   diff against its **Pattern**, **Why it is a defect** and **Correct form**. A scan pattern does not
   find all instances: **Detection** says what it misses, so read the diff for those too.

4. **Verify each lead.** For each scan hit and each instance you suspect, read the code at that
   site. Report only true instances, each with `file:line`, the class id, why it is an instance, and
   the correct form.

5. **Check the known non-issues.** Before you report, read the "Known non-issues" section of the
   README. Do not report a pattern that it lists.

6. **Question the tests and the design.** Tests can encode bad behaviour. Do not accept a test as
   proof that the code is correct. Question the design as it is, and what it could be.

7. **Report.** For each confirmed new instance, say so and suggest an `instances.toml` row (after
   the fix ships). For a defect that fits no class, say "candidate new class" and give a one-line
   pattern.

This skill does not change the catalogue. To add classes or instances, use the
`defect-catalogue-add` skill.
