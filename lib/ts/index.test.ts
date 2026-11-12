import assert from "node:assert/strict";
import test from "node:test";
import { execFile } from "node:child_process";
import { mkdir, mkdtemp, readFile, realpath, rename, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import {
	ACTIONS,
	ACTION_CAPABILITIES,
	ACTION_GROUPS,
	ACTION_NAMES,
	SubspaceBindingError,
	actionArgs,
	actionEffect,
	runAction,
} from "./index.js";

const capabilities = JSON.stringify(ACTION_CAPABILITIES);

function runner(results: Array<{ stdout: string; stderr?: string; code?: number }>) {
	const calls: Array<{ command: string; args: string[]; cwd: string; signal?: AbortSignal }> = [];
	const exec = async (command: string, args: string[], options: { cwd: string; signal?: AbortSignal }) => {
		calls.push({ command, args, ...options });
		const next = results.shift();
		if (!next) throw new Error("Unexpected execution.");
		return { stdout: next.stdout, stderr: next.stderr ?? "", code: next.code ?? 0, killed: false };
	};
	return { calls, exec };
}

test("defines every action once and no generic Git action", () => {
	const grouped = Object.values(ACTION_GROUPS).flat();
	assert.equal(new Set(grouped).size, grouped.length);
	assert.deepEqual([...grouped].sort(), [...ACTION_NAMES].sort());
	assert.deepEqual(Object.keys(ACTIONS).sort(), [...ACTION_NAMES].sort());
	assert.equal(ACTION_NAMES.includes("git" as never), false);
	assert.equal("comments" in ACTION_GROUPS, false);
	for (const action of ["commentList", "feedbackAdd", "instructionAdd"]) {
		assert.equal(ACTION_NAMES.includes(action as never), false);
	}
	for (const action of ACTION_NAMES) {
		const capability = ACTION_CAPABILITIES.actions[action];
		assert.ok(capability);
		assert.ok(capability.group);
		assert.ok(capability.effect);
		assert.ok(Array.isArray(capability.fields));
		assert.ok(Array.isArray(capability.arguments));
		assert.ok(capability.result);
	}
});

test("validates typed workspace ranges and repository paths", () => {
	assert.throws(
		() => actionArgs({ action: "log", from: "main", to: "topic", rangeMode: "symmetric", gitArgs: ["--stat"] }),
		/no gitArgs field/,
	);
	assert.throws(
		() => actionArgs({ action: "diff", from: "main", to: "topic", rangeMode: "mergeBase", paths: ["../secret"] }),
		/repository-relative/,
	);
	assert.throws(
		() => actionArgs({ action: "diff", from: "main", to: "topic", rangeMode: "symmetric" }),
		/needs rangeMode mergeBase/,
	);
	assert.throws(
		() => actionArgs({ action: "log", from: "main", to: "topic", rangeMode: "mergeBase" }),
		/needs rangeMode symmetric/,
	);
});

test("rejects invalid field relationships before execution", async () => {
	const invalid = [
		{ action: "default", set: "" },
		{ action: "branchPrefix", set: "x", unset: true },
		{ action: "workspaceBranch", set: "x", unset: true },
		{ action: "add", name: "x", parent: "main", parentOf: "topic" },
		{ action: "add", name: "x", parentOf: "topic", ref: "main" },
		{ action: "branch", name: "x", branch: "x", detach: true },
		{ action: "merge", mergeSource: "x", deleteBranch: true },
	] as const;
	for (const call of invalid) {
		let count = 0;
		await assert.rejects(
			runAction(call as never, {
				cwd: "/tmp",
				exec: async () => {
					count += 1;
					throw new Error("called");
				},
			}),
			SubspaceBindingError,
		);
		assert.equal(count, 0);
	}
});

test("runs reads through the protocol check and validates results", async () => {
	const { calls, exec } = runner([
		{ stdout: capabilities },
		{ stdout: JSON.stringify({ project: "tide", name: "main" }), stderr: "warning" },
	]);
	const run = await runAction({ action: "current" }, { exec, cwd: "/tmp/tide" });
	assert.equal(run.error, undefined);
	assert.deepEqual(run.result, { project: "tide", name: "main" });
	assert.equal(run.stderr, "warning");
	assert.equal(calls.length, 2);
});

test("runs mutations directly without a preview or token", async () => {
	const { calls, exec } = runner([
		{ stdout: capabilities },
		{
			stdout: JSON.stringify({
				project: "tide",
				workspace: "new",
				previous: "old",
				path: "/new",
				previous_path: "/old",
			}),
		},
	]);
	const run = await runAction({ action: "rename", target: "old", newName: "new" }, { exec, cwd: "/tmp" });
	assert.equal(run.error, undefined);
	assert.equal(calls.length, 2);
	assert.equal(calls[1]?.args.includes("--preview"), false);
});

test("treats a recorded branch read as a read and updates as writes", () => {
	assert.equal(actionEffect({ action: "workspaceBranch" }), "read");
	assert.equal(actionEffect({ action: "workspaceBranch", set: "work/feature" }), "write");
	assert.equal(actionEffect({ action: "workspaceBranch", unset: true }), "write");
});

test("merge modes pass the full-range flag to the CLI", () => {
	for (const action of ["merge", "mergePreview"] as const) {
		assert.equal(actionArgs({ action, mergeSource: "work" }).includes("--all"), false);
		assert.equal(actionArgs({ action, mergeSource: "work", all: true }).includes("--all"), true);
	}
});

test("remove passes the force flag only when requested", () => {
	assert.equal(actionArgs({ action: "remove", target: "work" }).includes("--force"), false);
	assert.equal(actionArgs({ action: "remove", target: "work", force: true }).includes("--force"), true);
});

test("maps generic recovery actions to the integration command", () => {
	const global = ["--json", "--no-interactive", "--color", "never"];
	assert.deepEqual(actionArgs({ action: "integrationContinue" }), [...global, "integration", "continue"]);
	assert.deepEqual(actionArgs({ action: "integrationAbort" }), [...global, "integration", "abort"]);
});

test("runs explicit previews without a token", async () => {
	for (const action of ["mergePreview", "rebasePreview"] as const) {
		const { calls, exec } = runner([
			{ stdout: capabilities },
			{ stdout: JSON.stringify({ plan: { effects: {}, warnings: [], confirmation: "Apply?" } }) },
		]);
		const run = await runAction(
			{ action, ...(action === "mergePreview" ? { mergeSource: "work" } : {}) },
			{ exec, cwd: "/tmp" },
		);
		assert.equal(run.error, undefined);
		assert.equal(calls.length, 2);
		assert.equal(calls[1]?.args.filter((arg) => arg === "--preview").length, 1);
		assert.deepEqual(run.result, { plan: { effects: {}, warnings: [], confirmation: "Apply?" } });
	}
});

test("rejects a token in an explicit preview", async () => {
	const invalid = runner([
		{ stdout: capabilities },
		{ stdout: JSON.stringify({ plan: { effects: {}, warnings: [], confirmation: "Apply?", token: "old" } }) },
	]);
	await assert.rejects(
		runAction({ action: "mergePreview", mergeSource: "work" }, { exec: invalid.exec, cwd: "/tmp" }),
		/The mergePreview preview is invalid/,
	);
});

test("returns malformed results as machine errors", async () => {
	const malformed = runner([{ stdout: capabilities }, { stdout: "bad", stderr: "parse warning", code: 1 }]);
	const run = await runAction({ action: "current" }, { exec: malformed.exec, cwd: "/tmp" });
	assert.equal(run.error?.code, "invalid-json");
	assert.equal(run.stderr, "parse warning");
});

test("runs the binding against the real CLI for reads, errors, and cancellation", async () => {
	const binary = process.env.SUBSPACE_TEST_CLI;
	assert.ok(binary, "The test needs a built Subspace CLI.");
	const dir = await realpath(await mkdtemp(join(tmpdir(), "subspace-binding-")));
	const invoke = promisify(execFile);
	let executions = 0;
	const exec = async (command: string, args: string[], options: { cwd: string; signal?: AbortSignal }) => {
		executions += 1;
		try {
			const result = await invoke(command, args, {
				cwd: options.cwd,
				signal: options.signal,
				env: {
					...process.env,
					HOME: dir,
					XDG_CONFIG_HOME: join(dir, "config"),
					XDG_STATE_HOME: join(dir, "state"),
				},
			});
			return { ...result, code: 0, killed: false };
		} catch (error) {
			const failure = error as Error & { code?: number; killed?: boolean; stdout?: string; stderr?: string };
			return {
				stdout: failure.stdout ?? "",
				stderr: failure.stderr ?? "",
				code: typeof failure.code === "number" ? failure.code : 1,
				killed: failure.killed ?? options.signal?.aborted ?? false,
			};
		}
	};
	try {
		const options = { exec, cwd: dir, command: binary };
		const list = await runAction({ action: "projectList" }, options);
		assert.deepEqual(list.result, {});
		assert.equal(list.code, 0);
		const missing = await runAction({ action: "projectCurrent" }, options);
		assert.equal(missing.error?.code, "failed");
		assert.notEqual(missing.code, 0);

		const projects = join(dir, "projects");
		const moved = join(dir, "moved");
		await mkdir(projects);
		await mkdir(moved);
		const hookConfig = join(dir, "config/subspace/config.json");
		await mkdir(join(dir, "config/subspace"), { recursive: true });
		await writeFile(hookConfig, JSON.stringify({ hooks: { project_add_pre: "false" } }));
		const rejectedInit = await runAction({ action: "projectInit", path: join(projects, "demo") }, options);
		assert.ok(rejectedInit.error);
		assert.deepEqual((await runAction({ action: "projectList" }, options)).result, {});
		await writeFile(hookConfig, "{}");
		const initialized = await runAction({ action: "projectInit", path: join(projects, "demo") }, options);
		assert.equal(initialized.error, undefined, JSON.stringify(initialized.error));
		let root = join(projects, "demo");
		await writeFile(join(root, "worktrees/main/tracked.txt"), "dirty\n");
		await writeFile(join(root, "worktrees/main/untracked.txt"), "untracked\n");
		const collision = await runAction({ action: "projectMove", name: "demo", dir: projects }, options);
		assert.ok(collision.error);
		assert.equal(await readFile(join(root, "worktrees/main/tracked.txt"), "utf8"), "dirty\n");
		await writeFile(hookConfig, JSON.stringify({ hooks: { project_move_pre: "false" } }));
		const rejectedMove = await runAction({ action: "projectMove", name: "demo", dir: moved }, options);
		assert.ok(rejectedMove.error);
		assert.deepEqual((await runAction({ action: "projectList" }, options)).result, { demo: root });
		assert.equal(await readFile(join(root, "worktrees/main/tracked.txt"), "utf8"), "dirty\n");
		await writeFile(hookConfig, "{}");
		const relocation = await runAction({ action: "projectMove", name: "demo", dir: moved }, options);
		assert.equal(relocation.error, undefined, JSON.stringify(relocation.error));
		root = join(moved, "demo");
		assert.equal(await readFile(join(root, "worktrees/main/untracked.txt"), "utf8"), "untracked\n");
		await writeFile(hookConfig, JSON.stringify({ hooks: { project_rename_post: "false" } }));
		const failedPost = await runAction({ action: "projectRename", name: "demo", newName: "transient" }, options);
		assert.ok(failedPost.error);
		root = join(moved, "transient");
		assert.equal(await readFile(join(root, "worktrees/main/tracked.txt"), "utf8"), "dirty\n");
		assert.deepEqual((await runAction({ action: "projectList" }, options)).result, { transient: root });
		await writeFile(hookConfig, "{}");
		const renamed = await runAction({ action: "projectRename", name: "transient", newName: "renamed" }, options);
		assert.equal(renamed.error, undefined, JSON.stringify(renamed.error));
		root = join(moved, "renamed");
		assert.equal(await readFile(join(root, "worktrees/main/tracked.txt"), "utf8"), "dirty\n");
		assert.equal(await readFile(join(root, "worktrees/main/untracked.txt"), "utf8"), "untracked\n");
		assert.deepEqual((await runAction({ action: "projectList" }, options)).result, { renamed: root });

		await invoke(
			"git",
			[
				"-C",
				join(root, "worktrees/main"),
				"-c",
				"user.name=Test",
				"-c",
				"user.email=test@example.com",
				"commit",
				"--allow-empty",
				"-m",
				"test",
			],
			{ cwd: dir },
		);
		const child = await runAction(
			{ action: "add", name: "child", parent: "main", ref: "HEAD" },
			{ ...options, cwd: join(root, "worktrees/main") },
		);
		assert.equal(child.error, undefined, JSON.stringify(child.error));
		const grandchild = await runAction(
			{ action: "add", name: "grandchild", parent: "child", ref: "HEAD" },
			{ ...options, cwd: join(root, "worktrees/child") },
		);
		assert.equal(grandchild.error, undefined, JSON.stringify(grandchild.error));
		const quote = (value: string) => "'" + value.replaceAll("'", "'\\''") + "'";
		await writeFile(
			hookConfig,
			JSON.stringify({
				hooks: { workspace_remove_pre: `${quote(binary)} --json -P renamed default --set grandchild` },
			}),
		);
		const changedPlan = await runAction(
			{ action: "remove", project: "renamed", target: "child", recursive: true },
			options,
		);
		assert.ok(changedPlan.error);
		assert.deepEqual((await runAction({ action: "default", project: "renamed" }, options)).result, {
			project: "renamed",
			setting: "default",
			value: "grandchild",
		});
		assert.deepEqual((await runAction({ action: "list", project: "renamed" }, options)).result, [
			{ name: "child", parent: "main" },
			{ name: "grandchild", parent: "child" },
			{ name: "main", parent: null },
		]);
		const reset = await runAction({ action: "default", project: "renamed", set: "main" }, options);
		assert.equal(reset.error, undefined, JSON.stringify(reset.error));
		await writeFile(hookConfig, JSON.stringify({ hooks: { workspace_remove_pre: "false" } }));
		const stopped = await runAction(
			{ action: "remove", project: "renamed", target: "child", recursive: true },
			options,
		);
		assert.ok(stopped.error);
		assert.ok(await readFile(join(root, "worktrees/child/.git"), "utf8"));
		assert.ok(await readFile(join(root, "worktrees/grandchild/.git"), "utf8"));
		assert.deepEqual((await runAction({ action: "list", project: "renamed" }, options)).result, [
			{ name: "child", parent: "main" },
			{ name: "grandchild", parent: "child" },
			{ name: "main", parent: null },
		]);
		await writeFile(hookConfig, JSON.stringify({ hooks: { workspace_remove_post: "false" } }));
		const removed = await runAction(
			{ action: "remove", project: "renamed", target: "child", recursive: true },
			options,
		);
		assert.ok(removed.error);
		const remaining = await runAction({ action: "list", project: "renamed" }, options);
		assert.deepEqual(remaining.result, [{ name: "main", parent: null }]);

		const controller = new AbortController();
		controller.abort();
		const before = executions;
		await assert.rejects(
			runAction({ action: "projectList" }, { ...options, signal: controller.signal }),
			(error: unknown) => error instanceof SubspaceBindingError && error.code === "cancelled",
		);
		assert.equal(executions, before);
	} finally {
		await rm(dir, { recursive: true, force: true });
	}
});

async function projectFixture() {
	const binary = process.env.SUBSPACE_TEST_CLI;
	assert.ok(binary, "The test needs a built Subspace CLI.");
	const dir = await realpath(await mkdtemp(join(tmpdir(), "subspace-project-")));
	const repo = join(dir, "demo");
	const invoke = promisify(execFile);
	const env: NodeJS.ProcessEnv = {
		...process.env,
		HOME: dir,
		XDG_CONFIG_HOME: join(dir, "config"),
		XDG_STATE_HOME: join(dir, "state"),
		GIT_CONFIG_GLOBAL: "/dev/null",
		GIT_CONFIG_SYSTEM: "/dev/null",
		GIT_CONFIG_COUNT: "1",
		GIT_CONFIG_KEY_0: "init.defaultBranch",
		GIT_CONFIG_VALUE_0: "main",
	};
	const cli = async (...args: string[]) => {
		try {
			const result = await invoke(binary, ["--json", ...args], { cwd: dir, env });
			return { code: 0, result: JSON.parse(result.stdout) };
		} catch (error) {
			const failure = error as Error & { code?: number; stdout?: string };
			return { code: failure.code ?? 1, result: JSON.parse(failure.stdout ?? "{}") };
		}
	};
	const initialized = await cli("project", "init", repo);
	assert.equal(initialized.code, 0, JSON.stringify(initialized.result));
	const worktree = join(repo, "worktrees/main");
	const git = (...args: string[]) => invoke("git", ["-C", worktree, ...args], { env });
	await writeFile(join(worktree, "tracked.txt"), "dirty\n");
	await writeFile(join(worktree, "staged.txt"), "staged\n");
	await git("add", "staged.txt");
	await writeFile(join(worktree, "untracked.txt"), "untracked\n");
	const config = join(dir, "config/subspace/config.json");
	await mkdir(join(dir, "config/subspace"), { recursive: true });
	return { dir, repo, worktree, config, git, cli, env };
}

test("project creation does not seed authored config", async () => {
	const { dir } = await projectFixture();
	try {
		const projectConfig = join(dir, "config/subspace/projects/demo");
		for (const file of ["manifest.json", "env.json", "gitconfig"]) {
			await assert.rejects(stat(join(projectConfig, file)), { code: "ENOENT" });
		}
	} finally {
		await rm(dir, { recursive: true, force: true });
	}
});

test("a failed registry write restores a moved project directory", async () => {
	const { dir, repo, config, cli } = await projectFixture();
	try {
		const destination = join(dir, "destination");
		await mkdir(destination);
		await writeFile(
			config,
			JSON.stringify({
				hooks: {
					project_move_pre:
						'mv "$XDG_STATE_HOME/subspace/projects.json" "$XDG_STATE_HOME/subspace/projects.saved" && mkdir "$XDG_STATE_HOME/subspace/projects.json"',
				},
			}),
		);
		const failed = await cli("project", "move", "demo", destination);
		assert.notEqual(failed.code, 0);
		assert.equal(await readFile(join(repo, "worktrees/main/tracked.txt"), "utf8"), "dirty\n");
		await assert.rejects(stat(join(destination, "demo")), { code: "ENOENT" });
		const registry = join(dir, "state/subspace/projects.json");
		await rm(registry, { recursive: true });
		await rename(join(dir, "state/subspace/projects.saved"), registry);
		await writeFile(config, "{}");
		const moved = await cli("project", "move", "demo", destination);
		assert.equal(moved.code, 0, JSON.stringify(moved.result));
		assert.equal(await readFile(join(destination, "demo/worktrees/main/tracked.txt"), "utf8"), "dirty\n");
	} finally {
		await rm(dir, { recursive: true, force: true });
	}
});

test("a failed state move restores the project and config directories", async () => {
	const { dir, repo, config, cli } = await projectFixture();
	try {
		await writeFile(
			config,
			JSON.stringify({
				hooks: {
					project_rename_pre:
						'mkdir -p "$XDG_STATE_HOME/subspace/projects/renamed" && printf blocked > "$XDG_STATE_HOME/subspace/projects/renamed/marker"',
				},
			}),
		);
		const failed = await cli("project", "rename", "demo", "renamed");
		assert.notEqual(failed.code, 0);
		assert.equal(await readFile(join(repo, "worktrees/main/tracked.txt"), "utf8"), "dirty\n");
		await assert.rejects(stat(join(dir, "renamed")), { code: "ENOENT" });
		assert.ok((await stat(join(dir, "config/subspace/projects/demo"))).isDirectory());
		await assert.rejects(stat(join(dir, "config/subspace/projects/renamed")), { code: "ENOENT" });
		await rm(join(dir, "state/subspace/projects/renamed"), { recursive: true });
		await writeFile(config, "{}");
		const renamed = await cli("project", "rename", "demo", "renamed");
		assert.equal(renamed.code, 0, JSON.stringify(renamed.result));
		assert.equal(await readFile(join(dir, "renamed/worktrees/main/tracked.txt"), "utf8"), "dirty\n");
	} finally {
		await rm(dir, { recursive: true, force: true });
	}
});

test("a failed registry write restores renamed project state", async () => {
	const { dir, repo, config, cli } = await projectFixture();
	try {
		await writeFile(
			config,
			JSON.stringify({
				hooks: {
					project_rename_pre:
						'mv "$XDG_STATE_HOME/subspace/projects.json" "$XDG_STATE_HOME/subspace/projects.saved" && mkdir "$XDG_STATE_HOME/subspace/projects.json"',
				},
			}),
		);
		const failed = await cli("project", "rename", "demo", "renamed");
		assert.notEqual(failed.code, 0);
		assert.equal(await readFile(join(repo, "worktrees/main/tracked.txt"), "utf8"), "dirty\n");
		await assert.rejects(stat(join(dir, "renamed")), { code: "ENOENT" });
		assert.ok((await stat(join(dir, "config/subspace/projects/demo"))).isDirectory());
		await assert.rejects(stat(join(dir, "config/subspace/projects/renamed")), { code: "ENOENT" });
		assert.ok((await stat(join(dir, "state/subspace/projects/demo"))).isDirectory());
		await assert.rejects(stat(join(dir, "state/subspace/projects/renamed")), { code: "ENOENT" });
		const registry = join(dir, "state/subspace/projects.json");
		await rm(registry, { recursive: true });
		await rename(join(dir, "state/subspace/projects.saved"), registry);
		await writeFile(config, "{}");
		const renamed = await cli("project", "rename", "demo", "renamed");
		assert.equal(renamed.code, 0, JSON.stringify(renamed.result));
		assert.equal(await readFile(join(dir, "renamed/worktrees/main/tracked.txt"), "utf8"), "dirty\n");
	} finally {
		await rm(dir, { recursive: true, force: true });
	}
});

test("rejects an incompatible protocol and contract", async () => {
	const wrong = runner([{ stdout: JSON.stringify({ ...ACTION_CAPABILITIES, version: 3 }) }]);
	await assert.rejects(runAction({ action: "current" }, { exec: wrong.exec, cwd: "/tmp" }), /Incompatible/);
	const missing = runner([{ stdout: JSON.stringify({ ...ACTION_CAPABILITIES, actions: {} }) }]);
	await assert.rejects(runAction({ action: "current" }, { exec: missing.exec, cwd: "/tmp" }), /no current action/);
});
