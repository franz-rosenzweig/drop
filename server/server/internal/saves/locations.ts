import fs from "node:fs";
import path from "node:path";
import { load } from "js-yaml";
import {
  MetadataSource,
  Platform,
  SaveLocationSource,
} from "~/prisma/client/enums";
import { systemConfig } from "../config/sys-conf";
import prisma from "../db/database";

/**
 * Where games keep their saves. Locations are Ludusavi-style path templates
 * ("<base>/www/save", "<winAppData>/Studio/Game") so the client can resolve
 * them per platform. Suggestions come from two places:
 *  - the Ludusavi manifest (https://github.com/mtkennerly/ludusavi-manifest),
 *    matched by Steam app id only, since names collide between games;
 *  - the game's own file list, for engines with a fixed save folder (RPG Maker).
 */

const MANIFEST_URL =
  "https://raw.githubusercontent.com/mtkennerly/ludusavi-manifest/master/data/manifest.yaml";
const MANIFEST_MAX_AGE_MS = 7 * 24 * 60 * 60 * 1000;

export type SaveLocationInput = {
  path: string;
  platform: Platform | null;
  source: SaveLocationSource;
};

type LudusaviFile = {
  tags?: string[];
  when?: Array<{ os?: string; store?: string }>;
} | null;
type LudusaviEntry = {
  files?: Record<string, LudusaviFile>;
  steam?: { id?: number };
} | null;

const LUDUSAVI_OS: Record<string, Platform> = {
  windows: Platform.Windows,
  mac: Platform.macOS,
  linux: Platform.Linux,
};

// Steam app id → that game's Ludusavi file entries, built once per download.
let ludusaviCache:
  | { mtimeMs: number; bySteamId: Map<string, Record<string, LudusaviFile>> }
  | undefined;

async function manifestPath(): Promise<string> {
  const dir = path.join(systemConfig.getDataFolder(), "ludusavi");
  const file = path.join(dir, "manifest.yaml");
  const fresh =
    fs.existsSync(file) &&
    Date.now() - fs.statSync(file).mtimeMs < MANIFEST_MAX_AGE_MS;
  if (fresh) return file;
  try {
    const response = await fetch(MANIFEST_URL);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(`${file}.tmp`, Buffer.from(await response.arrayBuffer()));
    fs.renameSync(`${file}.tmp`, file);
  } catch (e) {
    // A stale copy is better than none.
    if (!fs.existsSync(file)) throw e;
  }
  return file;
}

async function ludusaviBySteamId() {
  const file = await manifestPath();
  const { mtimeMs } = fs.statSync(file);
  if (ludusaviCache?.mtimeMs === mtimeMs) return ludusaviCache.bySteamId;

  const manifest = load(fs.readFileSync(file, "utf-8")) as Record<
    string,
    LudusaviEntry
  >;
  const bySteamId = new Map<string, Record<string, LudusaviFile>>();
  for (const entry of Object.values(manifest)) {
    const id = entry?.steam?.id;
    if (id !== undefined && entry?.files) bySteamId.set(`${id}`, entry.files);
  }
  ludusaviCache = { mtimeMs, bySteamId };
  return bySteamId;
}

/** A platform for a path whose Ludusavi entry doesn't name one. */
function guessPlatform(template: string): Platform | null {
  if (
    template.startsWith("<win") ||
    template.startsWith("<home>/AppData/") ||
    template.startsWith("<home>/Saved Games/")
  )
    return Platform.Windows;
  if (template.startsWith("<home>/Library/")) return Platform.macOS;
  if (template.startsWith("<xdg") || template.startsWith("<home>/."))
    return Platform.Linux;
  return null;
}

function fromLudusavi(
  files: Record<string, LudusaviFile>,
): SaveLocationInput[] {
  const out: SaveLocationInput[] = [];
  for (const [template, file] of Object.entries(files)) {
    if (!file?.tags?.includes("save")) continue;
    // Steam's own install and userdata folders don't exist for a Drop copy.
    if (template.includes("<root>") || template.includes("<storeGameId>"))
      continue;
    const whens = file.when ?? [];
    const platforms = new Set<Platform | null>();
    if (whens.length === 0 || whens.some((w) => !w.os))
      platforms.add(guessPlatform(template));
    for (const w of whens) {
      const platform = w.os ? LUDUSAVI_OS[w.os] : undefined;
      if (platform) platforms.add(platform);
    }
    for (const platform of platforms)
      out.push({
        path: template,
        platform,
        source: SaveLocationSource.Ludusavi,
      });
  }
  return out;
}

/** Save folders fixed by the game's engine, found from its versions' file lists. */
async function fromEngine(gameId: string): Promise<SaveLocationInput[]> {
  const hasFile = async (file: string) =>
    !!(await prisma.gameVersion.findFirst({
      where: { gameId, fileList: { has: file } },
      select: { versionId: true },
    }));
  // RPG Maker MV keeps saves next to its www folder; MZ (and MV built
  // without www/) next to data/.
  if (await hasFile("www/data/System.json"))
    return [
      {
        path: "<base>/www/save",
        platform: null,
        source: SaveLocationSource.Engine,
      },
    ];
  if (await hasFile("data/System.json"))
    return [
      {
        path: "<base>/save",
        platform: null,
        source: SaveLocationSource.Engine,
      },
    ];
  return [];
}

function dedupe(locations: SaveLocationInput[]) {
  // A path that applies on any platform makes its per-platform copies redundant.
  const anywhere = new Set(
    locations.filter((l) => l.platform === null).map((l) => l.path),
  );
  const seen = new Set<string>();
  return locations.filter((l) => {
    if (l.platform !== null && anywhere.has(l.path)) return false;
    const key = `${l.platform ?? "*"}|${l.path}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

/** Suggested save locations for one game. Nothing is stored. */
export async function suggestSaveLocations(
  gameId: string,
): Promise<SaveLocationInput[]> {
  const game = await prisma.game.findUnique({
    where: { id: gameId },
    select: { metadataSource: true, metadataId: true },
  });
  if (!game) return [];

  const out: SaveLocationInput[] = [];
  if (game.metadataSource === MetadataSource.Steam) {
    const files = (await ludusaviBySteamId()).get(game.metadataId);
    if (files) out.push(...fromLudusavi(files));
  }
  out.push(...(await fromEngine(gameId)));
  return dedupe(out);
}
