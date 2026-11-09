export type SubspaceExecResult = {
	stdout: string;
	stderr: string;
	code: number;
	killed?: boolean;
};

export type SubspaceExec = (
	command: string,
	args: string[],
	options: { cwd: string; signal?: AbortSignal },
) => Promise<SubspaceExecResult>;

export class SubspaceBindingError extends Error {
	constructor(
		public readonly code: string,
		message: string,
		public readonly details?: unknown,
	) {
		super(message);
		this.name = "SubspaceBindingError";
	}
}

export const PROTOCOL = "subspace-json";
export const PROTOCOL_VERSION = 2;
export const FEATURES = ["action-contract", "direct-mutation", "mutation-preview"] as const;

export const ACTION_GROUPS = {
	reads: [
		"current",
		"path",
		"root",
		"parent",
		"children",
		"siblings",
		"list",
		"show",
		"inspectWorkspace",
		"inspectProject",
		"compare",
		"tree",
		"diff",
		"log",
		"projectCurrent",
		"projectList",
		"snapshotList",
	],
	managedWrites: [
		"projectRemove",
		"projectRename",
		"default",
		"branchPrefix",
		"workspaceBranch",
		"repair",
		"projectMove",
		"projectClone",
		"projectInit",
		"add",
		"branch",
		"remove",
		"rename",
	],
	integrations: [
		"mergePreview",
		"merge",
		"rebasePreview",
		"rebase",
		"integrationContinue",
		"integrationAbort",
		"snapshotCreate",
		"snapshotRestore",
	],
	commands: [],
} as const;

export type ActionGroup = keyof typeof ACTION_GROUPS;
export type ActionName = (typeof ACTION_GROUPS)[ActionGroup][number];
export type ActionEffect = "read" | "write" | "command";
export type ActionCall = { action: ActionName } & Record<string, unknown>;

export type ActionCapability = {
	group: ActionGroup;
	effect: ActionEffect | "conditional";
	fields: readonly string[];
	required: readonly string[];
	arguments: readonly string[];
	result: "object" | "array" | "any";
};

function actionConstraints(action: ActionName): readonly string[] {
	const constraints: Partial<Record<ActionName, readonly string[]>> = {
		diff: ["typed-range", "repository-relative-paths"],
		log: ["typed-range", "repository-relative-paths"],
		branchPrefix: ["set-or-unset"],
		workspaceBranch: ["set-or-unset"],
		add: ["parent-or-parentOf", "parentOf-excludes-ref", "branch-excludes-detach"],
		branch: ["branch-excludes-detach"],
		merge: ["deleteBranch-requires-remove"],
		mergePreview: ["deleteBranch-requires-remove"],
	};
	return constraints[action] ?? [];
}

type ActionDefinition = ActionCapability & {
	args(params: ActionCall): string[];
	resultKeys?: readonly string[];
};

const pos = (value: unknown): string[] => (typeof value === "string" && value.length > 0 ? [value] : []);
const opt = (flag: string, value: unknown): string[] =>
	typeof value === "string" && value.length > 0 ? [flag, value] : [];
const flag = (name: string, value: unknown): string[] => (value === true ? [name] : []);

const scopeFields = ["project", "projectScope", "workspace"] as const;
const f = (fields: readonly string[], required: readonly string[] = []) => ({ fields, required });
const read = (
	group: ActionGroup,
	arguments_: readonly string[],
	args: (params: ActionCall) => string[],
	fields: readonly string[] = [],
	required: readonly string[] = [],
	result: "object" | "array" | "any" = "object",
	resultKeys: readonly string[] = [],
): ActionDefinition => ({
	group,
	effect: "read",
	...f(fields, required),
	arguments: arguments_,
	args,
	result,
	resultKeys,
});
const write = (
	group: ActionGroup,
	arguments_: readonly string[],
	args: (params: ActionCall) => string[],
	fields: readonly string[] = [],
	required: readonly string[] = [],
	result: "object" | "array" | "any" = "object",
	resultKeys: readonly string[] = [],
): ActionDefinition => ({
	group,
	effect: "write",
	...f(fields, required),
	arguments: arguments_,
	args,
	result,
	resultKeys,
});

function rangeArgs(p: ActionCall): string[] {
	const output = p.output === "nameOnly" ? "name-only" : p.output;
	const paths = Array.isArray(p.paths) ? p.paths.flatMap((path) => ["--path", String(path)]) : [];
	return [
		"--from",
		String(p.from),
		"--to",
		String(p.to),
		...(typeof output === "string" ? ["--output", output] : []),
		...paths,
	];
}

function newWorkspaceArgs(p: ActionCall): string[] {
	return [
		...pos(p.name),
		...pos(p.ref),
		...opt("--branch", p.branch),
		...flag("--detach", p.detach),
		...flag("--unstaged", p.unstaged),
	];
}

const rangeFields = ["from", "to", "rangeMode", "output", "paths"] as const;
const newWorkspaceFields = ["name", "ref", "branch", "detach", "unstaged"] as const;

export const ACTIONS = {
	current: read("reads", ["current"], () => ["current"], [], [], "object", ["project", "name"]),
	path: read("reads", ["path"], () => ["path"], [], [], "object", ["path", "scope"]),
	root: read("reads", ["root"], () => ["root"], [], [], "object", ["name"]),
	parent: read("reads", ["parent"], () => ["parent"], [], [], "object", ["name"]),
	children: read("reads", ["children"], () => ["children"], [], [], "array"),
	siblings: read("reads", ["siblings"], () => ["siblings"], [], [], "array"),
	default: {
		...read("managedWrites", ["default"], (p) => ["default", ...opt("--set", p.set)], ["set"], [], "object", [
			"project",
			"setting",
			"value",
		]),
		effect: "conditional",
	},
	branchPrefix: {
		...read(
			"managedWrites",
			["branch-prefix"],
			(p) => ["branch-prefix", ...opt("--set", p.set), ...flag("--unset", p.unset)],
			["set", "unset"],
			[],
			"object",
			["project", "setting", "value"],
		),
		effect: "conditional",
	},
	workspaceBranch: {
		...read(
			"managedWrites",
			["workspace-branch"],
			(p) => ["workspace-branch", ...opt("--set", p.set), ...flag("--unset", p.unset)],
			["set", "unset"],
			[],
			"object",
			["project", "workspace", "setting", "value"],
		),
		effect: "conditional",
	},
	list: read("reads", ["list"], (p) => ["list", ...flag("--parents", p.parents)], ["parents"], [], "array"),
	show: read("reads", ["show"], () => ["show"], [], [], "any"),
	inspectWorkspace: read("reads", ["inspect", "workspace"], () => ["inspect", "workspace"], [], [], "object", [
		"project",
		"name",
		"path",
		"head",
		"status",
	]),
	inspectProject: read("reads", ["inspect", "project"], () => ["inspect", "project"], [], [], "object", [
		"project",
		"path",
		"workspaces",
		"repair",
	]),
	compare: read("reads", ["compare"], (p) => ["compare", ...pos(p.other)], ["other"], ["other"], "object", [
		"project",
		"current",
		"workspace",
		"commits",
		"statistics",
		"paths",
		"dirty_paths",
		"rewritten",
	]),
	tree: read("reads", ["tree"], () => ["tree"], [], [], "array"),
	diff: read("reads", ["diff"], (p) => ["diff", ...rangeArgs(p)], rangeFields, ["from", "to", "rangeMode"], "object", [
		"command",
		"range",
		"args",
		"stdout",
	]),
	log: read("reads", ["log"], (p) => ["log", ...rangeArgs(p)], rangeFields, ["from", "to", "rangeMode"], "object", [
		"command",
		"range",
		"args",
		"stdout",
	]),
	projectCurrent: read("reads", ["project", "current"], () => ["project", "current"], [], [], "object", [
		"project",
		"path",
	]),
	projectList: read("reads", ["project", "list"], () => ["project", "list"], [], [], "object"),
	projectRemove: write(
		"managedWrites",
		["project", "remove"],
		(p) => ["project", "remove", ...pos(p.name), ...flag("--remove-directory", p.removeDirectory)],
		["name", "removeDirectory"],
		["name"],
		"object",
		["project", "path", "removed"],
	),
	projectRename: write(
		"managedWrites",
		["project", "rename"],
		(p) => ["project", "rename", ...pos(p.name), ...pos(p.newName), ...flag("--no-move", p.noMove)],
		["name", "newName", "noMove"],
		["name", "newName"],
		"object",
		["project", "previous", "path", "previous_path"],
	),
	projectMove: write(
		"managedWrites",
		["project", "move"],
		(p) => ["project", "move", ...pos(p.name), ...pos(p.dir)],
		["name", "dir"],
		["name", "dir"],
		"object",
		["project", "path", "previous_path"],
	),
	projectClone: write(
		"managedWrites",
		["project", "clone"],
		(p) => ["project", "clone", ...pos(p.remote), ...pos(p.dir), ...opt("--name", p.name)],
		["remote", "dir", "name"],
		["remote"],
		"object",
		["project", "path", "workspace"],
	),
	projectInit: write(
		"managedWrites",
		["project", "init"],
		(p) => ["project", "init", ...pos(p.path), ...opt("--name", p.name)],
		["path", "name"],
		["path"],
		"object",
		["project", "path", "workspace"],
	),
	add: write(
		"managedWrites",
		["add"],
		(p) => ["add", ...newWorkspaceArgs(p), ...opt("--parent", p.parent), ...opt("--parent-of", p.parentOf)],
		[...newWorkspaceFields, "parent", "parentOf"],
		["name"],
		"object",
		["project", "workspace", "path", "branch", "parent", "reparented"],
	),
	branch: write(
		"managedWrites",
		["branch"],
		(p) => ["branch", ...newWorkspaceArgs(p)],
		newWorkspaceFields,
		["name"],
		"object",
		["project", "workspace", "path", "branch", "parent", "reparented"],
	),
	remove: write(
		"managedWrites",
		["remove"],
		(p) => ["remove", ...pos(p.target), ...flag("--keep-branch", p.keepBranch), ...flag("--recursive", p.recursive)],
		["target", "keepBranch", "recursive"],
		["target"],
		"object",
		["project", "removed", "reparented"],
	),
	rename: write(
		"managedWrites",
		["rename"],
		(p) => ["rename", ...pos(p.target), ...pos(p.newName)],
		["target", "newName"],
		["target", "newName"],
		"object",
		["project", "workspace", "previous", "path", "previous_path"],
	),
	repair: {
		...write("managedWrites", ["repair"], (p) => ["repair", ...flag("--dry-run", p.dryRun)], ["dryRun"], [], "object", [
			"project",
			"dry_run",
			"links_checked",
			"relinked",
			"unrepairable",
			"pruned",
			"reparented",
		]),
		effect: "conditional",
	},
	mergePreview: read(
		"integrations",
		["merge", "--preview"],
		(p) => [
			"merge",
			...pos(p.mergeSource),
			...opt("--squash", p.squash),
			...flag("--all", p.all),
			...flag("--unstaged", p.unstaged),
			...flag("--remove", p.remove),
			...flag("--delete-branch", p.deleteBranch),
			"--preview",
		],
		["mergeSource", "squash", "all", "unstaged", "remove", "deleteBranch"],
		["mergeSource"],
		"object",
		["plan"],
	),
	merge: write(
		"integrations",
		["merge"],
		(p) => [
			"merge",
			...pos(p.mergeSource),
			...opt("--squash", p.squash),
			...flag("--all", p.all),
			...flag("--unstaged", p.unstaged),
			...flag("--remove", p.remove),
			...flag("--delete-branch", p.deleteBranch),
		],
		["mergeSource", "squash", "all", "unstaged", "remove", "deleteBranch"],
		["mergeSource"],
		"object",
		["project", "workspace", "source", "squash", "commits"],
	),
	rebasePreview: read(
		"integrations",
		["rebase", "--preview"],
		(p) => ["rebase", ...pos(p.base), ...flag("--all", p.all), "--preview"],
		["base", "all"],
		[],
		"object",
		["plan"],
	),
	rebase: write(
		"integrations",
		["rebase"],
		(p) => ["rebase", ...pos(p.base), ...flag("--all", p.all)],
		["base", "all"],
		[],
		"object",
		["project", "workspace", "base", "base_head", "previous_head", "outcome"],
	),
	integrationContinue: write(
		"integrations",
		["integration", "continue"],
		() => ["integration", "continue"],
		[],
		[],
		"object",
		["project", "workspace", "operation", "outcome", "head"],
	),
	integrationAbort: write("integrations", ["integration", "abort"], () => ["integration", "abort"], [], [], "object", [
		"project",
		"workspace",
		"operation",
		"outcome",
		"head",
	]),
	snapshotCreate: write(
		"integrations",
		["snapshot", "create"],
		(p) => ["snapshot", "create", ...pos(p.archiveRef)],
		["archiveRef"],
		[],
		"object",
		["workspace", "sha", "path", "bytes"],
	),
	snapshotList: read("reads", ["snapshot", "list"], () => ["snapshot", "list"], [], [], "array"),
	snapshotRestore: write(
		"integrations",
		["snapshot", "restore"],
		(p) => ["snapshot", "restore", ...pos(p.snapshotFile), ...pos(p.name)],
		["snapshotFile", "name"],
		["snapshotFile"],
		"object",
		["project", "workspace", "path", "branch", "parent", "reparented"],
	),
} as const satisfies Record<ActionName, ActionDefinition>;

export const ACTION_NAMES = Object.keys(ACTIONS) as ActionName[];
export const ACTION_CAPABILITIES = {
	protocol: PROTOCOL,
	version: PROTOCOL_VERSION,
	features: FEATURES,
	groups: ACTION_GROUPS,
	actions: Object.fromEntries(
		ACTION_NAMES.map((name) => {
			const { args: _args, resultKeys, ...capability } = ACTIONS[name];
			return [
				name,
				{
					description: `Run the ${name} machine action.`,
					...capability,
					constraints: actionConstraints(name),
					resultKeys: resultKeys ?? [],
				},
			];
		}),
	),
};

export function actionEffect(call: ActionCall): ActionEffect {
	if (call.action === "default") return call.set === undefined ? "read" : "write";
	if (call.action === "branchPrefix" || call.action === "workspaceBranch")
		return call.set === undefined && call.unset !== true ? "read" : "write";
	if (call.action === "repair") return call.dryRun === true ? "read" : "write";
	const effect = ACTIONS[call.action].effect;
	return effect === "conditional" ? "read" : effect;
}

function record(value: unknown): Record<string, unknown> | undefined {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

function checkCall(call: ActionCall): ActionDefinition {
	const definition: ActionDefinition | undefined = ACTIONS[call.action];
	if (!definition) throw new SubspaceBindingError("invalid-call", `Unknown Subspace action: ${String(call.action)}.`);
	const allowed = new Set(["action", ...scopeFields, ...definition.fields]);
	for (const key of Object.keys(call))
		if (!allowed.has(key))
			throw new SubspaceBindingError("invalid-call", `The ${call.action} action has no ${key} field.`);
	for (const key of definition.required)
		if (call[key] === undefined)
			throw new SubspaceBindingError("invalid-call", `The ${call.action} action needs a ${key} field.`);
	if (
		(call.action === "default" || call.action === "branchPrefix" || call.action === "workspaceBranch") &&
		call.set !== undefined &&
		(typeof call.set !== "string" || call.set.length === 0)
	)
		throw new SubspaceBindingError("invalid-call", `The ${call.action} action needs a non-empty set value.`);
	if (call.action === "diff" && call.rangeMode !== "mergeBase")
		throw new SubspaceBindingError("invalid-call", "The diff action needs rangeMode mergeBase.");
	if (call.action === "log" && call.rangeMode !== "symmetric")
		throw new SubspaceBindingError("invalid-call", "The log action needs rangeMode symmetric.");
	if (
		(call.action === "diff" || call.action === "log") &&
		call.output !== undefined &&
		!["patch", "stat", "nameOnly"].includes(String(call.output))
	)
		throw new SubspaceBindingError("invalid-call", `The ${call.action} output is invalid.`);
	if (
		(call.action === "diff" || call.action === "log") &&
		call.paths !== undefined &&
		(!Array.isArray(call.paths) ||
			call.paths.some((path) => typeof path !== "string" || path.startsWith("/") || path.split("/").includes("..")))
	)
		throw new SubspaceBindingError("invalid-call", `The ${call.action} paths must be repository-relative paths.`);
	if (
		(call.action === "branchPrefix" || call.action === "workspaceBranch") &&
		call.set !== undefined &&
		call.unset === true
	)
		throw new SubspaceBindingError("invalid-call", `The ${call.action} action cannot set and unset together.`);
	if (call.action === "add" && call.parent !== undefined && call.parentOf !== undefined)
		throw new SubspaceBindingError("invalid-call", "The add action cannot set parent and parentOf together.");
	if (call.action === "add" && call.parentOf !== undefined && call.ref !== undefined)
		throw new SubspaceBindingError("invalid-call", "The add action cannot set parentOf and ref together.");
	if ((call.action === "add" || call.action === "branch") && call.branch !== undefined && call.detach === true)
		throw new SubspaceBindingError("invalid-call", `The ${call.action} action cannot set branch and detach together.`);
	if ((call.action === "merge" || call.action === "mergePreview") && call.deleteBranch === true && call.remove !== true)
		throw new SubspaceBindingError("invalid-call", `The ${call.action} action needs remove when deleteBranch is set.`);
	return definition;
}

const GLOBAL_FLAGS = ["--json", "--no-interactive", "--color", "never"] as const;

function scopeArgs(call: ActionCall): string[] {
	return [...opt("--project", call.project), ...flag("-p", call.projectScope), ...opt("--workspace", call.workspace)];
}

export function actionArgs(call: ActionCall): string[] {
	const definition = checkCall(call);
	return [...GLOBAL_FLAGS, ...scopeArgs(call), ...definition.args(call)];
}

function checkResult(action: ActionName, result: unknown): void {
	const definition = ACTIONS[action];
	if (definition.result === "any") return;
	if (definition.result === "array") {
		if (!Array.isArray(result))
			throw new SubspaceBindingError("invalid-result", `The ${action} result is not a JSON array.`);
		return;
	}
	const value = record(result);
	if (!value) throw new SubspaceBindingError("invalid-result", `The ${action} result is not a JSON object.`);
	for (const key of definition.resultKeys ?? [])
		if (!(key in value)) throw new SubspaceBindingError("invalid-result", `The ${action} result has no ${key} field.`);
}

export type MachineError = { code: string; message: string; details?: unknown };
export type MachineRun = {
	action: ActionName;
	result?: unknown;
	error?: MachineError;
	stderr: string;
	args: string[];
	code: number;
};
export type RunOptions = { exec: SubspaceExec; cwd: string; signal?: AbortSignal; command?: string };

function parseMachine(action: ActionName, run: SubspaceExecResult, args: string[], validate: boolean): MachineRun {
	let value: unknown;
	try {
		value = JSON.parse(run.stdout);
	} catch {
		return {
			action,
			error: { code: "invalid-json", message: `The ${action} call gave no JSON result.` },
			stderr: run.stderr,
			args,
			code: run.code,
		};
	}
	const envelope = record(value);
	const error = envelope && record(envelope.error);
	if (error) {
		if (typeof error.code !== "string" || typeof error.message !== "string")
			return {
				action,
				error: { code: "invalid-result", message: `The ${action} error result is invalid.` },
				stderr: run.stderr,
				args,
				code: run.code,
			};
		return {
			action,
			error: {
				code: error.code,
				message: error.message,
				...(error.details === undefined ? {} : { details: error.details }),
			},
			stderr: run.stderr,
			args,
			code: run.code,
		};
	}
	if (run.code !== 0)
		return {
			action,
			error: { code: "exit", message: `The ${action} call exited with status ${run.code}.` },
			stderr: run.stderr,
			args,
			code: run.code,
		};
	try {
		if (validate) checkResult(action, value);
	} catch (error) {
		return {
			action,
			error: { code: "invalid-result", message: error instanceof Error ? error.message : String(error) },
			stderr: run.stderr,
			args,
			code: run.code,
		};
	}
	return { action, result: value, stderr: run.stderr, args, code: run.code };
}

async function execute(
	args: string[],
	action: ActionName,
	options: RunOptions,
	validate: boolean,
): Promise<MachineRun> {
	if (options.signal?.aborted) throw new SubspaceBindingError("cancelled", `The ${action} call was cancelled.`);
	const run = await options.exec(options.command ?? "subspace", args, {
		cwd: options.cwd,
		...(options.signal ? { signal: options.signal } : {}),
	});
	if (options.signal?.aborted || run.killed)
		throw new SubspaceBindingError("cancelled", `The ${action} call was cancelled.`);
	return parseMachine(action, run, args, validate);
}

function checkCapabilities(value: unknown): void {
	const data = record(value);
	if (!data || data.protocol !== PROTOCOL || data.version !== PROTOCOL_VERSION)
		throw new SubspaceBindingError(
			"incompatible-protocol",
			`Incompatible Subspace protocol. Expected ${PROTOCOL} v${PROTOCOL_VERSION}.`,
		);
	if (!Array.isArray(data.features) || FEATURES.some((feature) => !(data.features as unknown[]).includes(feature)))
		throw new SubspaceBindingError("incompatible-protocol", "The Subspace protocol has no required feature set.");
	const groups = record(data.groups);
	const actions = record(data.actions);
	if (!groups || !actions)
		throw new SubspaceBindingError("incompatible-protocol", "The Subspace protocol has no action contract.");
	for (const [group, names] of Object.entries(ACTION_GROUPS)) {
		if (JSON.stringify(groups[group]) !== JSON.stringify(names))
			throw new SubspaceBindingError("incompatible-protocol", `The Subspace ${group} action group is incompatible.`);
	}
	for (const action of ACTION_NAMES) {
		const capability = record(actions[action]);
		if (!capability)
			throw new SubspaceBindingError("incompatible-protocol", `The Subspace protocol has no ${action} action.`);
		if (capability.group !== ACTIONS[action].group || capability.effect !== ACTIONS[action].effect)
			throw new SubspaceBindingError("incompatible-protocol", `The Subspace ${action} action is incompatible.`);
		if (
			typeof capability.description !== "string" ||
			JSON.stringify(capability.fields) !== JSON.stringify(ACTIONS[action].fields) ||
			JSON.stringify(capability.required) !== JSON.stringify(ACTIONS[action].required) ||
			JSON.stringify(capability.constraints) !== JSON.stringify(actionConstraints(action)) ||
			!Array.isArray(capability.arguments) ||
			capability.result !== ACTIONS[action].result
		)
			throw new SubspaceBindingError("incompatible-protocol", `The Subspace ${action} contract is invalid.`);
	}
}

export async function capabilities(options: RunOptions): Promise<unknown> {
	const args = [...GLOBAL_FLAGS, "capabilities"];
	if (options.signal?.aborted)
		throw new SubspaceBindingError("cancelled", "The Subspace capabilities check was cancelled.");
	const run = await options.exec(options.command ?? "subspace", args, {
		cwd: options.cwd,
		...(options.signal ? { signal: options.signal } : {}),
	});
	if (options.signal?.aborted || run.killed)
		throw new SubspaceBindingError("cancelled", "The Subspace capabilities check was cancelled.");
	if (run.code !== 0)
		throw new SubspaceBindingError("unavailable", run.stderr.trim() || "The Subspace capabilities check failed.");
	let value: unknown;
	try {
		value = JSON.parse(run.stdout);
	} catch {
		throw new SubspaceBindingError("invalid-result", "The Subspace capabilities check gave no JSON result.");
	}
	checkCapabilities(value);
	return value;
}

function checkPreview(action: ActionName, result: unknown): void {
	const data = record(result);
	const plan = record(data?.plan) ?? data;
	if (
		!plan ||
		plan.effects === undefined ||
		!Array.isArray(plan.warnings) ||
		plan.warnings.some((item) => typeof item !== "string") ||
		typeof plan.confirmation !== "string" ||
		"token" in plan
	)
		throw new SubspaceBindingError("invalid-result", `The ${action} preview is invalid.`);
}

export async function runAction(call: ActionCall, options: RunOptions): Promise<MachineRun> {
	checkCall(call);
	await capabilities(options);
	const run = await execute(actionArgs(call), call.action, options, true);
	if (!run.error && call.action.endsWith("Preview")) checkPreview(call.action, run.result);
	return run;
}
