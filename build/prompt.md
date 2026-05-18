This application has been migrated from PatternFly 5 to PatternFly 6 using automated tools. Pattern-based and LLM-assisted fixes have already been applied. Your job is to get the application into a building and working state.

Ensure all @patternfly/* packages are upgraded to PatternFly 6 versions and lock files are consistent with the updated dependencies.

## Step 1: Build and collect all errors

Identify the project's actual build command by inspecting its build configuration. Use this exact command for ALL build verification throughout this process — do not substitute with other tools, as they may have different settings and miss real errors. Collect the FULL list of compilation and type errors.

## Step 2: Group errors

Use your best judgement to group the errors by root cause. Group errors together logically.

## Step 3: Fix errors in batches by groups

For each group of errors, research the correct fix ONCE, then apply it across ALL affected files before moving on to the next category. Do NOT run the build after every single file change.

When fixing errors, consult the PatternFly 6 API docs if needed:
- https://www.patternfly.org/get-started/upgrade/

## Step 4: Verify build

After fixing ALL categories, run the build again. If new errors appear, repeat steps 2-4. Aim to get to a clean build in 2-3 iterations.

## Step 5: Fix tests

After the build succeeds, identify and run the project's test command. Apply the same batch approach: collect all failures, group by cause, fix in batches, then re-run.

Update snapshots if the new output is correct.

## Critical: Do NOT revert to PatternFly 5

Automated tools have already migrated imports, components, props, and CSS tokens to PatternFly 6. When build errors arise from these migrations, fix them using the correct PatternFly 6 API. Never undo the migration by reverting code back to PatternFly 5 patterns, imports, or packages.

## Important

- Do not refactor, improve, or add features. Only fix what is broken.
- Do not modify code that already builds and passes tests.
- Make minimal, targeted fixes.
- Do NOT run the build after every file change. Fix a whole category first, then build.
