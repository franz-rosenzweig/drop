import aclManager from "~/server/internal/acls";
import prisma from "~/server/internal/db/database";

export default defineEventHandler(async (h3) => {
  const allowed = await aclManager.allowSystemACL(h3, ["game:read"]);
  if (!allowed) throw createError({ statusCode: 403 });

  const id = getRouterParam(h3, "id")!;
  const game = await prisma.game.findUnique({
    where: { id },
    select: {
      saveLocationsLocked: true,
      saveLocations: {
        select: { id: true, path: true, platform: true, source: true },
        orderBy: { path: "asc" },
      },
    },
  });
  if (!game) throw createError({ statusCode: 404, message: "Game not found" });
  return { locked: game.saveLocationsLocked, locations: game.saveLocations };
});
