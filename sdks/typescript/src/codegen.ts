import { readFileSync, writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const SCALAR_MAP: Record<string, string> = {
  Int: "number",
  Float: "number",
  String: "string",
  Boolean: "boolean",
  ID: "string",
  JSON: "unknown",
};

export interface ParsedField {
  name: string;
  tsType: string;
  optional: boolean;
}

export interface ParsedObject {
  kind: "type" | "input";
  name: string;
  fields: ParsedField[];
}

export interface ParsedEnum {
  name: string;
  values: string[];
}

export interface ParsedSchema {
  enums: ParsedEnum[];
  objects: ParsedObject[];
}

function mapGraphqlTypeToTs(rawType: string): { tsType: string; optional: boolean } {
  const trimmed = rawType.replace(/@[^\s]+(\([^)]*\))?/g, "").trim();
  const nonNull = trimmed.endsWith("!");
  const inner = nonNull ? trimmed.slice(0, -1).trim() : trimmed;
  if (inner.startsWith("[") && inner.endsWith("]")) {
    const elem = mapGraphqlTypeToTs(inner.slice(1, -1));
    return { tsType: `Array<${elem.tsType}>`, optional: !nonNull };
  }
  const baseName = inner.replace(/!$/, "");
  const tsBase = SCALAR_MAP[baseName] ?? baseName;
  return { tsType: nonNull ? tsBase : `${tsBase} | null`, optional: !nonNull };
}

function stripBlockComments(sdl: string): string {
  return sdl
    .replace(/"""[\s\S]*?"""/g, "")
    .replace(/#[^\n]*/g, "");
}

export function parseSdlToModel(sdl: string): ParsedSchema {
  const cleaned = stripBlockComments(sdl);
  const enums: ParsedEnum[] = [];
  const objects: ParsedObject[] = [];

  const enumRe = /\benum\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{([^}]*)\}/g;
  for (const match of cleaned.matchAll(enumRe)) {
    const values = match[2]
      .split(/\s+/)
      .map((v) => v.trim())
      .filter((v) => v.length > 0 && !v.startsWith("@"));
    enums.push({ name: match[1], values });
  }

  const objRe = /\b(type|input)\s+([A-Za-z_][A-Za-z0-9_]*)[^{]*\{([^}]*)\}/g;
  for (const match of cleaned.matchAll(objRe)) {
    const kind = match[1] as "type" | "input";
    const name = match[2];
    const body = match[3];
    const fields: ParsedField[] = [];
    for (const rawLine of body.split(/\r?\n/)) {
      const line = rawLine.trim();
      if (!line) continue;
      // Match fieldName(args): Type or fieldName: Type
      const fieldMatch = line.match(
        /^([A-Za-z_][A-Za-z0-9_]*)(?:\([^)]*\))?\s*:\s*([^\s@=]+(?:\s*!\s*|\s*\]\s*!?)?)/,
      );
      if (!fieldMatch) continue;
      const mapped = mapGraphqlTypeToTs(fieldMatch[2]);
      fields.push({
        name: fieldMatch[1],
        tsType: mapped.tsType,
        optional: mapped.optional,
      });
    }
    objects.push({ kind, name, fields });
  }

  return { enums, objects };
}

export function generateTypesFromSdl(sdl: string, sourceLabel = "supergraph.graphql"): string {
  const model = parseSdlToModel(sdl);
  const out: string[] = [
    `// Code generated from ${sourceLabel} by @gap-protocol/sdk codegen. DO NOT EDIT BY HAND.`,
    "",
  ];

  for (const en of model.enums) {
    if (en.values.length === 0) {
      out.push(`export type ${en.name} = string;`);
    } else {
      const union = en.values.map((v) => JSON.stringify(v)).join(" | ");
      out.push(`export type ${en.name} = ${union};`);
    }
    out.push("");
  }

  for (const obj of model.objects) {
    out.push(`export interface ${obj.name} {`);
    for (const field of obj.fields) {
      const q = field.optional ? "?" : "";
      out.push(`  ${field.name}${q}: ${field.tsType};`);
    }
    out.push("}");
    out.push("");
  }

  return `${out.join("\n").trimEnd()}\n`;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const args = process.argv.slice(2);
  const inIdx = args.indexOf("--sdl");
  const outIdx = args.indexOf("--out");
  if (inIdx !== -1 && args[inIdx + 1]) {
    const sdlText = readFileSync(args[inIdx + 1], "utf8");
    const rendered = generateTypesFromSdl(sdlText, args[inIdx + 1]);
    if (outIdx !== -1 && args[outIdx + 1]) {
      writeFileSync(args[outIdx + 1], rendered, "utf8");
    } else {
      process.stdout.write(rendered);
    }
  } else {
    process.stdout.write("Usage: node src/codegen.ts --sdl <schema.graphql> [--out <types.ts>]\n");
  }
}
