import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

type Theme = {
	bold(text: string): string;
	fg(color: "toolTitle" | "accent" | "muted" | "toolOutput" | "warning" | "error" | "success", text: string): string;
};

type Content = { type: string; text?: string };

type ToolResult = { content: readonly Content[]; details?: unknown };

export type ResultTruncation = {
	by: "lines" | "bytes";
	totalLines: number;
	totalBytes: number;
	maxLines: number;
	maxBytes: number;
	path: string;
};

function record(value: unknown): Record<string, unknown> | undefined {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

function json(value: unknown): string {
	return JSON.stringify(value) ?? "null";
}

function logicalLines(value: unknown): number {
	if (typeof value === "string")
		return value.length === 0 ? 0 : value.split("\n").length - Number(value.endsWith("\n"));
	if (Array.isArray(value)) return value.reduce<number>((total, item) => total + logicalLines(item), 0);
	const fields = record(value);
	if (fields) return Object.values(fields).reduce<number>((total, item) => total + logicalLines(item), 0);
	return 0;
}

/** Saves a complete tool result when it exceeds Pi's context limits. */
export function limitJson(
	value: unknown,
	options: { directory?: string; prefix?: string; fileName?: string; maxLines: number; maxBytes: number },
): { text: string; truncated?: ResultTruncation } {
	const text = json(value);
	const totalLines = logicalLines(value);
	const totalBytes = Buffer.byteLength(text, "utf8");
	if (totalLines <= options.maxLines && totalBytes <= options.maxBytes) return { text };
	const directory = mkdtempSync(join(options.directory ?? tmpdir(), options.prefix ?? "subspace-result-"));
	const path = join(directory, options.fileName ?? "result.json");
	writeFileSync(path, text, { encoding: "utf8", mode: 0o600 });
	return {
		text,
		truncated: {
			by: totalBytes > options.maxBytes ? "bytes" : "lines",
			totalLines,
			totalBytes,
			maxLines: options.maxLines,
			maxBytes: options.maxBytes,
			path,
		},
	};
}

/** Renders one Subspace tool call. */
export function renderToolCall(
	tool: string,
	call: Record<string, unknown>,
	theme: Theme,
	options: { expanded?: boolean },
): string {
	let text = theme.fg("toolTitle", theme.bold(tool));
	if (typeof call.action === "string") text += ` ${theme.fg("accent", call.action)}`;
	for (const [key, value] of Object.entries(call)) {
		if (key === "action" || value === undefined) continue;
		const raw = json(value);
		const preview = raw.length > 80 ? `${raw.slice(0, 77)}...` : raw;
		text += ` ${theme.fg("muted", `${key}=`)}${theme.fg("accent", preview)}`;
	}
	if (options.expanded) text += `\n${theme.fg("toolOutput", JSON.stringify(call, null, 2))}`;
	return text;
}

/** Renders a Subspace result or its private spill-file location. */
export function renderToolResult(
	result: ToolResult,
	theme: Theme,
	options: { expanded?: boolean; isPartial?: boolean; isError?: boolean; summary?: (value: unknown) => string },
): string {
	if (options.isPartial) return "";
	const text = result.content
		.filter((part) => part.type === "text")
		.map((part) => part.text ?? "")
		.join("\n");
	let value: unknown;
	try {
		value = JSON.parse(text);
	} catch {
		value = text;
	}
	if (options.isError) return theme.fg("error", typeof value === "string" ? value : JSON.stringify(value, null, 2));
	if (options.expanded) return theme.fg("toolOutput", JSON.stringify(value, null, 2));
	const truncated = record(record(result.details)?.truncated);
	if (typeof truncated?.path === "string")
		return theme.fg("warning", `${truncated.totalLines} lines, ${truncated.totalBytes} bytes. ${truncated.path}`);
	const error = record(record(value)?.error);
	if (typeof error?.message === "string") return theme.fg("warning", error.message);
	return theme.fg("success", options.summary?.(value) ?? json(value));
}
