import { tmpdir } from "node:os";
import { StringEnum } from "@earendil-works/pi-ai";
import { DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES } from "@earendil-works/pi-coding-agent";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Text } from "@earendil-works/pi-tui";
import { Type } from "typebox";
import type { TSchema } from "typebox";

import { ACTION_NAMES, runAction } from "../../lib/ts/index.js";
import { limitJson, renderToolCall, renderToolResult } from "./helpers.js";
import type { ResultTruncation } from "./helpers.js";

// The scope flags that each action shares. They select the project and the
// workspace that the command runs against.
const SCOPE = {
	project: Type.Optional(Type.String({ description: "Project name. It selects the project." })),
	projectScope: Type.Optional(
		Type.Boolean({
			description: "Use the project of the CWD without selecting a workspace. Do not combine it with workspace.",
		}),
	),
	workspace: Type.Optional(Type.String({ description: "Workspace name or @sigil." })),
};

// The typed range fields of `diff` and `log`.
const RANGE_FIELDS = {
	from: Type.String({ description: "Required for diff and log. Start workspace name or @sigil." }),
	to: Type.String({ description: "Required for diff and log. End workspace name or @sigil." }),
	rangeMode: StringEnum(["mergeBase", "symmetric"] as const, {
		description: "Required for diff and log. Use `mergeBase` for diff. Use `symmetric` for log. Never omit it.",
	}),
	output: Type.Optional(StringEnum(["patch", "stat", "nameOnly"] as const, { description: "Git output mode." })),
	paths: Type.Optional(Type.Array(Type.String(), { description: "Repository-relative paths to include." })),
};

// One call of the tool: the action name and the fields of that action.
export type Call = { action: ActionName } & Record<string, unknown>;

type SubspaceToolRun = {
	action: ActionName;
	result?: unknown;
	error?: CliError;
	stderr: string;
	args: string[];
	code: number;
};

type SubspaceToolBinding = {
	ACTION_NAMES: readonly ActionName[];
	runAction: typeof runAction;
};

export type Dependencies = {
	subspaceToolBinding?: SubspaceToolBinding;
};

function preview(value: unknown): string {
	const text = JSON.stringify(value) ?? String(value);
	return text.length > 80 ? `${text.slice(0, 77)}...` : text;
}

function record(value: unknown): Record<string, unknown> | undefined {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

function stringField(value: Record<string, unknown>, key: string): string | undefined {
	return typeof value[key] === "string" ? value[key] : undefined;
}

function arrayLength(value: Record<string, unknown>, key: string): number {
	return Array.isArray(value[key]) ? value[key].length : 0;
}

function lines(text: string): number {
	const trimmed = text.trimEnd();
	return trimmed.length === 0 ? 0 : trimmed.split("\n").length;
}

function count(count: number, singular: string): string {
	return `${count} ${singular}${count === 1 ? "" : "s"}`;
}

function previewWarnings(value: Record<string, unknown>): string {
	const warnings = Array.isArray(value["warnings"])
		? value["warnings"].filter((warning): warning is string => typeof warning === "string" && warning.length > 0)
		: [];
	return warnings.length > 0 ? `; Warnings: ${warnings.join("; ")}` : "";
}

function integrationPreviewSummary(action: "merge" | "rebase", value: Record<string, unknown>): string {
	const plan = record(value["plan"]);
	const effects = plan && record(plan["effects"]);
	const commits = count(arrayLength(plan ?? {}, "applied_commits"), "planned commit");
	const scope =
		action === "merge"
			? `${stringField(effects ?? {}, "source") ?? "Source"} -> ${stringField(effects ?? {}, "destination") ?? "workspace"}`
			: (stringField(effects ?? {}, "workspace") ?? "Workspace");
	return `Preview: ${scope}: ${action}, ${commits}${previewWarnings(plan ?? {})}`;
}

function workspaceSummary(value: Record<string, unknown>): string | undefined {
	const name = stringField(value, "name");
	const files = record(value["files"]);
	if (!name || !files) return undefined;
	const changes = ["added", "modified", "renamed", "deleted", "untracked", "unmerged"]
		.map((status) => [status, files[status]] as const)
		.filter((entry): entry is readonly [string, number] => typeof entry[1] === "number" && entry[1] > 0)
		.map(([status, amount]) => `${amount} ${status}`);
	changes.push(`${arrayLength(files, "staged")} staged`);
	return `${name}: ${changes.join(", ")}`;
}

function resultSummary(action: ActionName | undefined, value: unknown): string {
	if (Array.isArray(value)) {
		const noun: Partial<Record<ActionName, string>> = {
			children: "workspace",
			siblings: "workspace",
			list: "workspace",
			show: "workspace",
			tree: "workspace",
			snapshotList: "snapshot",
		};
		return count(value.length, action ? (noun[action] ?? "result") : "result");
	}
	if (typeof value === "string") return value.length > 0 ? preview(value) : "No output";
	const data = record(value);
	if (!data) return preview(value);

	switch (action) {
		case "current":
			return `${stringField(data, "project")}/${stringField(data, "name")}`;
		case "path":
			return stringField(data, "path") ?? "Path returned";
		case "root":
		case "parent":
			return stringField(data, "name") ?? "Workspace returned";
		case "default":
		case "branchPrefix":
		case "workspaceBranch": {
			const setting = stringField(data, "setting") ?? "Setting";
			return data["value"] === null ? `${setting}: unset` : `${setting}: ${String(data["value"])}`;
		}
		case "show":
			return workspaceSummary(data) ?? "Workspace status returned";
		case "inspectWorkspace":
			return `${stringField(data, "project")}/${stringField(data, "name")}`;
		case "inspectProject":
			return `${stringField(data, "project")}: project inspection`;
		case "compare":
			return `${stringField(data, "current") ?? "workspace"} vs ${stringField(data, "workspace") ?? "workspace"}`;
		case "diff":
		case "log": {
			const stdout = stringField(data, "stdout") ?? "";
			return stdout.length === 0 ? "No output" : count(lines(stdout), "output line");
		}
		case "projectList":
			return count(Object.keys(data).length, "project");
		case "projectCurrent":
			return `${stringField(data, "project") ?? "Project"}: ${stringField(data, "path") ?? "path returned"}`;
		case "projectClone":
		case "projectInit":
		case "add":
		case "branch":
		case "snapshotRestore":
			return `${stringField(data, "workspace") ?? "Workspace"} created`;
		case "projectRemove":
			return `${stringField(data, "project") ?? "Project"} removed`;
		case "projectRename":
		case "projectMove":
			return `${stringField(data, "project") ?? "Project"} updated`;
		case "remove":
			return count(arrayLength(data, "removed"), "removed workspace");
		case "rename":
			return `${stringField(data, "previous") ?? "Workspace"} -> ${stringField(data, "workspace") ?? "workspace"}`;
		case "repair":
			return `${arrayLength(data, "relinked")} relinked, ${arrayLength(data, "pruned")} pruned, ${arrayLength(data, "unrepairable")} unrepairable`;
		case "merge":
			return `${stringField(data, "source") ?? "Source"} -> ${stringField(data, "workspace") ?? "workspace"}: ${count(arrayLength(data, "commits"), "commit")}`;
		case "mergePreview":
			return integrationPreviewSummary("merge", data);
		case "rebase":
			return `${stringField(data, "workspace") ?? "Workspace"}: ${stringField(data, "outcome") ?? "rebased"}`;
		case "rebasePreview":
			return integrationPreviewSummary("rebase", data);
		case "integrationContinue":
		case "integrationAbort":
			return `${stringField(data, "workspace") ?? "Workspace"}: ${stringField(data, "outcome") ?? "rebased"}`;
		case "snapshotCreate":
			return `${stringField(data, "workspace") ?? "Workspace"}: ${String(data["bytes"])} bytes archived`;
	}

	const fields = Object.entries(data)
		.filter((entry) => entry[1] === null || ["string", "number", "boolean"].includes(typeof entry[1]))
		.slice(0, 3)
		.map(([key, item]) => `${key}=${preview(item)}`);
	return fields.length > 0 ? fields.join(" ") : `${Object.keys(data).length} result fields`;
}

// One action of the matrix and its fields.
type Spec = {
	fields: Record<string, TSchema>;
};

const NAME = Type.String({ description: "Name of the project or workspace, or the name to give a new one." });
const NEW_NAME = Type.String({ description: "New name to give it." });
const DIR = Type.String({
	description: "Project directory: the destination to move into, or the location for a new project.",
});
const SET = Type.Optional(Type.String({ minLength: 1, description: "Value to set." }));
const UNSET = Type.Optional(Type.Boolean({ description: "Clear the setting." }));
const UNSTAGED = Type.Optional(Type.Boolean({ description: "Carry unstaged and untracked files along." }));
const ALL = Type.Optional(
	Type.Boolean({
		description:
			"For merge, select commits since the Git merge base. For rebase, replay every commit ahead of the base.",
	}),
);

const NEW_WORKSPACE_FIELDS = {
	name: NAME,
	ref: Type.Optional(Type.String({ description: "Start point: a git ref or @sigil." })),
	branch: Type.Optional(Type.String({ description: "New branch name." })),
	detach: Type.Optional(Type.Boolean({ description: "Check out the start point with a detached HEAD." })),
	unstaged: UNSTAGED,
};

// Every action of the tool and its fields.
export const ACTIONS = {
	current: { fields: {} },
	path: { fields: {} },
	root: { fields: {} },
	parent: { fields: {} },
	children: { fields: {} },
	siblings: { fields: {} },
	default: { fields: { set: SET } },
	branchPrefix: { fields: { set: SET, unset: UNSET } },
	workspaceBranch: { fields: { set: SET, unset: UNSET } },
	list: {
		fields: { parents: Type.Optional(Type.Boolean({ description: "Add the parent of each workspace." })) },
	},
	show: { fields: {} },
	inspectWorkspace: { fields: {} },
	inspectProject: { fields: {} },
	compare: {
		fields: { other: Type.String({ description: "Workspace name or @sigil to compare with the current workspace." }) },
	},
	tree: { fields: {} },
	diff: { fields: RANGE_FIELDS },
	log: { fields: RANGE_FIELDS },
	projectCurrent: { fields: {} },
	projectList: { fields: {} },
	projectRemove: {
		fields: {
			name: NAME,
			removeDirectory: Type.Optional(Type.Boolean({ description: "Also delete the directory from disk." })),
		},
	},
	projectRename: {
		fields: {
			name: NAME,
			newName: NEW_NAME,
			noMove: Type.Optional(Type.Boolean({ description: "Leave the directory where it is." })),
		},
	},
	projectMove: {
		fields: {
			name: NAME,
			dir: DIR,
		},
	},
	projectClone: {
		fields: {
			remote: Type.String({ description: "Remote URL to clone." }),
			dir: Type.Optional(DIR),
			name: Type.Optional(NAME),
		},
	},
	projectInit: {
		fields: {
			path: Type.String({ description: "Path to the new project directory." }),
			name: Type.Optional(NAME),
		},
	},
	add: {
		fields: {
			...NEW_WORKSPACE_FIELDS,
			parent: Type.Optional(Type.String({ description: "Parent workspace name or @sigil." })),
			parentOf: Type.Optional(Type.String({ description: "Insert the new workspace as the parent of this one." })),
		},
	},
	branch: {
		fields: NEW_WORKSPACE_FIELDS,
	},
	remove: {
		fields: {
			target: Type.String({ description: "Workspace name or @sigil." }),
			keepBranch: Type.Optional(Type.Boolean({ description: "Keep the git branch." })),
			recursive: Type.Optional(Type.Boolean({ description: "Remove the workspace and its descendants." })),
		},
	},
	rename: {
		fields: {
			target: Type.String({ description: "Workspace name or @sigil." }),
			newName: NEW_NAME,
		},
	},
	repair: {
		fields: { dryRun: Type.Optional(Type.Boolean({ description: "Report what changes, and change nothing." })) },
	},
	mergePreview: {
		fields: {
			mergeSource: Type.String({ description: "Source workspace name or @sigil." }),
			squash: Type.Optional(Type.String({ description: "Commit message for a squash merge." })),
			all: ALL,
			unstaged: UNSTAGED,
			remove: Type.Optional(Type.Boolean({ description: "Remove the source workspace after the merge." })),
			deleteBranch: Type.Optional(Type.Boolean({ description: "Delete the source branch with the workspace." })),
		},
	},
	merge: {
		fields: {
			mergeSource: Type.String({ description: "Source workspace name or @sigil." }),
			squash: Type.Optional(Type.String({ description: "Commit message for a squash merge." })),
			all: ALL,
			unstaged: UNSTAGED,
			remove: Type.Optional(Type.Boolean({ description: "Remove the source workspace after the merge." })),
			deleteBranch: Type.Optional(Type.Boolean({ description: "Delete the source branch with the workspace." })),
		},
	},
	rebasePreview: {
		fields: {
			base: Type.Optional(Type.String({ description: "Base workspace name or @sigil." })),
			all: ALL,
		},
	},
	rebase: {
		fields: {
			base: Type.Optional(Type.String({ description: "Base workspace name or @sigil." })),
			all: ALL,
		},
	},
	integrationContinue: {
		fields: {},
	},
	integrationAbort: {
		fields: {},
	},
	snapshotCreate: {
		fields: { archiveRef: Type.Optional(Type.String({ description: "Git ref or @sigil to archive." })) },
	},
	snapshotList: { fields: {} },
	snapshotRestore: {
		fields: {
			snapshotFile: Type.String({ description: "Snapshot path, or a file name in the snapshots directory." }),
			name: Type.Optional(NAME),
		},
	},
} satisfies Record<string, Spec>;

export type ActionName = keyof typeof ACTIONS;

type RegisteredTool = {
	name: "subspace";
	actions: readonly ActionName[];
	executionMode: "sequential";
};

export const REGISTERED_TOOLS: readonly RegisteredTool[] = [
	{ name: "subspace", actions: Object.keys(ACTIONS) as ActionName[], executionMode: "sequential" },
];

// The fields of the selected actions, merged into one property set. Each
// field names its actions and marks the actions that require it. This keeps
// the flat, provider-compatible schema clear about valid action-field pairs.
function flatFields(actions: readonly ActionName[]): Record<string, TSchema> {
	const uses = new Map<string, { schema: TSchema; actions: string[]; required: Set<string> }>();
	for (const action of actions) {
		const spec = ACTIONS[action] as Spec;
		const required = new Set<string>(Type.Object(spec.fields).required ?? []);
		for (const [key, schema] of Object.entries(spec.fields)) {
			const use = uses.get(key);
			if (use && JSON.stringify(use.schema) !== JSON.stringify(schema))
				throw new Error(`The ${key} field has two different schemas, one of them from ${action}.`);
			const entry = use ?? { schema, actions: [], required: new Set<string>() };
			entry.actions.push(action);
			if (required.has(key)) entry.required.add(action);
			uses.set(key, entry);
		}
	}
	return Object.fromEntries(
		[...uses].map(([key, use]) => {
			const labels = use.actions.map((action) => `${action}${use.required.has(action) ? " (required)" : ""}`);
			const schema = use.schema as TSchema & { description?: unknown };
			const description = typeof schema.description === "string" ? ` ${schema.description}` : "";
			return [key, Type.Optional({ ...schema, description: `Actions: ${labels.join(", ")}.${description}` })];
		}),
	);
}

// The parameter schema stays flat because provider mappers do not all accept
// an action union. Field descriptions expose every valid action-field pair.
export function actionParams(actions: readonly ActionName[]): TSchema {
	return Type.Object(
		{
			action: StringEnum(actions, {
				description: "The Subspace operation. Each other field names the actions that accept it.",
			}),
			...SCOPE,
			...flatFields(actions),
		},
		{ description: "One Subspace operation. Common scope fields work with every action." },
	);
}

export const Params = actionParams(Object.keys(ACTIONS) as ActionName[]);

// The error object of a failed CLI result.
export type CliError = { code: string; message: string; details?: unknown };

// The exec function that the tool runs subspace through. It matches `pi.exec`.
export type Exec = (
	command: string,
	args: string[],
	options?: { signal?: AbortSignal; cwd?: string; timeout?: number },
) => Promise<{ stdout: string; stderr: string; code: number; killed: boolean }>;

// What one run needs beyond the call itself. Every field but `exec` has a default.
export type RunOptions = {
	exec: Exec;
	command?: string;
	cwd?: string;
	signal?: AbortSignal;
	maxLines?: number;
	maxBytes?: number;
	tempDir?: string;
};

// The stable document that every tool result sends to the model.
export type ResultEnvelope = {
	action: string;
	ok: boolean;
	result?: unknown;
	error?: CliError;
	diagnostics?: string;
	truncated?: Truncated;
};

// The outcome of one run: the stable JSON envelope, the parsed document, and run details.
export type Run = {
	text: string;
	result: unknown;
	stderr: string;
	args: string[];
	code: number;
	truncated?: Truncated;
};

// What a truncated result reports in place of the machine result: which limit
// it passed, its size, and the file that holds the complete JSON.
export type Truncated = ResultTruncation;

export function limitEnvelope(
	action: string,
	envelope: ResultEnvelope,
	options: RunOptions,
): { text: string; truncated?: Truncated } {
	const limited = limitJson(envelope, {
		directory: options.tempDir ?? tmpdir(),
		prefix: "subspace-tool-",
		fileName: `${action}.json`,
		maxLines: options.maxLines ?? DEFAULT_MAX_LINES,
		maxBytes: options.maxBytes ?? DEFAULT_MAX_BYTES,
	});
	if (!limited.truncated) return { text: limited.text };
	return {
		text: JSON.stringify({
			action,
			ok: envelope.ok,
			truncated: limited.truncated,
			...(envelope.diagnostics ? { diagnostics: `Full diagnostics: ${limited.truncated.path}` } : {}),
		}),
		truncated: limited.truncated,
	};
}

function outputRun(
	action: ActionName,
	result: unknown,
	stderr: string,
	args: string[],
	code: number,
	options: RunOptions,
	error?: CliError,
): Run {
	const envelope: ResultEnvelope = { action, ok: error === undefined, ...(error ? { error } : { result }) };
	if (stderr.length > 0) envelope.diagnostics = stderr;
	const cut = limitEnvelope(action, envelope, options);
	return { text: cut.text, result, stderr, args, code, ...(cut.truncated ? { truncated: cut.truncated } : {}) };
}

// Registers the Subspace tool with its local TypeScript binding.
export default function (pi: ExtensionAPI, dependencies: Dependencies = {}): void {
	pi.registerTool({
		name: "subspace",
		label: "Subspace",
		description: "Run one Subspace operation and return its JSON result.",
		promptSnippet: "Run Subspace operations",
		promptGuidelines: [
			"Pass only fields whose description names the selected action. Required fields are marked (required).",
			"For log, pass from, to, and rangeMode=symmetric. For diff, pass from, to, and rangeMode=mergeBase.",
			"Bad call: log with from and to but no rangeMode. Bad call: compare with output or paths. Compare accepts only other.",
		],
		parameters: actionParams(Object.keys(ACTIONS) as ActionName[]),
		executionMode: "sequential",
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const call = params as Call;
			const options: RunOptions = {
				exec: (command, args, runOptions) =>
					pi.exec(
						call.action === "integrationContinue" ? "env" : command,
						call.action === "integrationContinue" ? ["GIT_EDITOR=true", command, ...args] : args,
						{ ...runOptions, cwd: ctx.cwd, signal },
					),
				cwd: ctx.cwd,
				signal,
			};
			let run: SubspaceToolRun;
			try {
				const binding = dependencies.subspaceToolBinding ?? { ACTION_NAMES, runAction };
				if (!binding.ACTION_NAMES.includes(call.action)) throw new Error(`The binding has no ${call.action} action.`);
				run = await binding.runAction(call, { exec: options.exec, cwd: ctx.cwd, ...(signal ? { signal } : {}) });
			} catch (error) {
				const value = record(error);
				run = {
					action: call.action,
					error: {
						code: typeof value?.code === "string" ? value.code : "binding",
						message: error instanceof Error ? error.message : String(error),
						...(value?.details === undefined ? {} : { details: value.details }),
					},
					stderr: "",
					args: [],
					code: 1,
				};
			}
			const output = outputRun(call.action, run.result, run.stderr, run.args, run.code, options, run.error);
			return {
				content: [{ type: "text", text: output.text }],
				details: {
					action: call.action,
					args: run.args,
					stderr: output.truncated && run.stderr ? `Full diagnostics: ${output.truncated.path}` : run.stderr,
					...(output.truncated ? { truncated: output.truncated } : {}),
				},
			};
		},
		renderCall(args, theme, context) {
			return new Text(
				renderToolCall("subspace", args as Record<string, unknown>, theme, { expanded: context.expanded }),
				0,
				0,
			);
		},
		renderResult(result, { expanded, isPartial }, theme, context) {
			if (isPartial) return new Text("", 0, 0);
			const details = result.details as { action?: ActionName; truncated?: Truncated } | undefined;
			return new Text(
				renderToolResult(result, theme, {
					expanded,
					isPartial,
					isError: context.isError,
					summary: (value) => {
						const envelope = record(value);
						return resultSummary(details?.action, envelope && "result" in envelope ? envelope.result : value);
					},
				}),
				0,
				0,
			);
		},
	});
}
