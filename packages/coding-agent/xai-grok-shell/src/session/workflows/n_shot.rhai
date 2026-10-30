// Purpose: Fork the conversation into parallel attempts, each in its own git worktree.
// Input: args.prompt and args.n for identical shots, or args.shots for per-shot settings.
//     Each item in args.shots is a map with prompt, and optionally model and label.
// Output: A report that compares the shots. The parent session reads it and helps the
//     user select a shot, or combine shots.
// Method:
//     1. Make a list of shots from the args.
//     2. Run all shots in parallel. Each shot gets a copy of the conversation and a worktree.
//     3. Write a full report to scratch and complete with a short overview.
let meta = #{
    name: "n-shot",
    description: "Fork this conversation into N parallel attempts, each in its own git worktree, then compare them",
    phases: [
        #{ title: "Shots", detail: "Run every shot in parallel, each in an isolated worktree" },
        #{ title: "Join", detail: "Collect each shot's final message and worktree changes" },
    ],
};

let max_shots = 10;

fn usage() {
    "Run /n-shot <N> <prompt>, or pass args.prompt with args.n, or args.shots."
}

fn last_line(text) {
    let lines = text.split("\n");
    let line = lines[lines.len() - 1];
    line.trim();
    line
}

if args == () {
    pause("verification", usage());
}

let shots = [];
if args.shots != () {
    for shot in args.shots {
        let prompt = if shot.prompt != () { shot.prompt } else { args.prompt };
        if prompt == () || prompt == "" {
            pause("verification", "Every shot needs a prompt. " + usage());
        }
        shots.push(#{ prompt: prompt, model: shot.model, label: shot.label });
    }
} else {
    if args.prompt == () || args.prompt == "" {
        pause("verification", "No prompt was provided. " + usage());
    }
    let n = if args.n != () { args.n } else { 2 };
    if n < 1 {
        pause("verification", "N must be at least 1. " + usage());
    }
    for i in 0..n {
        shots.push(#{ prompt: args.prompt, model: (), label: () });
    }
}
if shots.len() == 0 {
    pause("verification", "No shots were provided. " + usage());
}
if shots.len() > max_shots {
    pause("verification", "At most " + max_shots.to_string() + " shots are allowed per run.");
}

phase("Shots");
let jobs = [];
let i = 0;
for shot in shots {
    let label = if shot.label != () && shot.label != "" {
        shot.label
    } else {
        "shot-" + (i + 1).to_string()
    };
    shots[i].label = label;
    let job = #{
        prompt: shot.prompt,
        label: label,
        phase: "Shots",
        fork_context: true,
        isolation_worktree: true,
        retain_worktree: true,
    };
    if shot.model != () && shot.model != "" {
        job.model = shot.model;
    }
    jobs.push(job);
    i += 1;
}
let results = parallel(jobs);

phase("Join");
let overview = "# n-shot: " + shots.len().to_string() + " shots finished\n\n";
let full = overview;
full += "Each shot started from a copy of this conversation and worked in its own git "
    + "worktree. Diffs use each shot's base commit and include committed and uncommitted "
    + "tracked changes, including changes the parent had when the run started. "
    + "Untracked files are listed separately and are not in the diff.\n\n"
    + "Guidance for the parent session:\n"
    + "- Compare and contrast the shots for the user, then give a recommendation.\n"
    + "- Inspect each shot with `git -C <worktree> diff --binary <base-commit> --` "
    + "and inspect its untracked files. If the base is unknown, do not assume there are no changes.\n"
    + "- Do not change the parent working tree until the user selects a shot or a combination.\n"
    + "- Before adoption, inspect the parent state and avoid overwriting existing changes. "
    + "Apply or port the selected tracked changes relative to the base commit and copy selected "
    + "untracked files. Resolve overlaps with parent changes explicitly.\n"
    + "- Keep the worktrees until the user is done with them.\n\n";

let idx = 0;
let succeeded = 0;
let entries = [];
for r in results {
    let shot = shots[idx];
    let heading = "## " + shot.label;
    if shot.model != () && shot.model != "" {
        heading += " (model: " + shot.model + ")";
    }
    let status = if r == () {
        "missing"
    } else if r.cancelled == true {
        "cancelled"
    } else if r.success == true {
        "done"
    } else {
        "failed"
    };
    if status == "done" {
        succeeded += 1;
    }
    let worktree = if r != () && r.worktree_path != () { r.worktree_path } else { "(none)" };
    let base = if r != () && r.worktree_base_commit != () { r.worktree_base_commit } else { "unknown" };
    let changes = if r != () && r.worktree_changes != () { r.worktree_changes } else { "unknown" };
    let output = if r != () && r.output != () { r.output } else { "" };
    if type_of(output) != "string" {
        output = json_encode(output);
    }

    entries.push(#{
        index: idx + 1,
        label: shot.label,
        agent_id: if r != () { r.agent_id } else { () },
        model: shot.model,
        prompt: shot.prompt,
        status: status,
        worktree: if worktree == "(none)" { () } else { worktree },
        changes: changes,
    });
    overview += "- " + shot.label + ": " + status + "; " + last_line(changes)
        + "; worktree " + worktree + "\n";
    full += heading + "\n\n- Status: " + status + "\n- Worktree: " + worktree
        + "\n- Base commit: " + base + "\n";
    if shot.prompt != args.prompt {
        full += "- Prompt:\n\n" + shot.prompt + "\n\n";
    }
    full += "- Changes:\n\n```\n" + changes + "\n```\n\n### Final message\n\n" + output + "\n\n";
    idx += 1;
}

let path = write_scratch_file("report.md", full);
write_scratch_file("shots.json", json_encode(#{
    objective: args.prompt,
    report: path,
    shots: entries,
}));
overview += "\nFull report: " + path + "\nRead it for each shot's final message and changed "
    + "files, then compare the shots for the user. After you apply the user's choice, call "
    + "the n_shot tool with only `resolve` (the absolute path of report.md from this message, "
    + "the outcome, the shot labels used, and a rationale) to record it.";
complete(#{
    path: path,
    report: overview,
    shots: shots.len(),
    succeeded: succeeded,
});
