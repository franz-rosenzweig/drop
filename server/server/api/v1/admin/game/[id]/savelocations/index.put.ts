import { type } from "arktype";
import { readDropValidatedBody, throwingArktype } from "~/server/arktype";
import aclManager from "~/server/internal/acls";
import prisma from "~/server/internal/db/database";

const SaveLocations = type({
  locations: type({
    path: "string > 0",
    platform: "'Windows' | 'macOS' | 'Linux' | null",
    source: "'Manual' | 'Ludusavi' | 'Engine' = 'Manual'",
  }).array(),
}).configure(throwingArktype);

// Replaces a game's save locations and locks the list, so the automatic fill
// task leaves it alone from now on.
export default defineEventHandler(async (h3) => {
  const allowed = await aclManager.allowSystemACL(h3, ["game:update"]);
  if (!allowed) throw createError({ statusCode: 403 });

  const id = getRouterParam(h3, "id")!;
  const body = await readDropValidatedBody(h3, SaveLocations);

  const { count } = await prisma.game.updateMany({
    where: { id },
    data: { saveLocationsLocked: true },
  });
  if (count === 0)
    throw createError({ statusCode: 404, message: "Game not found" });

  const seen = new Set<string>();
  const locations = body.locations
    .map((l) => ({ ...l, path: l.path.trim() }))
    .filter((l) => {
      const key = `${l.platform ?? "*"}|${l.path}`;
      if (!l.path || seen.has(key)) return false;
      seen.add(key);
      return true;
    });

  await prisma.$transaction([
    prisma.gameSaveLocation.deleteMany({ where: { gameId: id } }),
    prisma.gameSaveLocation.createMany({
      data: locations.map((l) => ({ ...l, gameId: id })),
    }),
  ]);

  return await prisma.gameSaveLocation.findMany({
    where: { gameId: id },
    select: { id: true, path: true, platform: true, source: true },
    orderBy: { path: "asc" },
  });
});
