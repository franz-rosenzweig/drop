import { ClientCapabilities } from "~/prisma/client/enums";
import { defineClientEventHandler } from "~/server/internal/clients/event-handler";
import prisma from "~/server/internal/db/database";

// Where this game keeps its saves, as path templates for the client to
// resolve. A null platform means the path applies on any platform.
export default defineClientEventHandler(async (h3, { fetchClient }) => {
  const client = await fetchClient();
  if (!client.capabilities.includes(ClientCapabilities.CloudSaves))
    throw createError({
      statusCode: 403,
      statusMessage: "Capability not allowed.",
    });

  const gameId = getRouterParam(h3, "gameid")!;
  return await prisma.gameSaveLocation.findMany({
    where: { gameId },
    select: { path: true, platform: true },
    orderBy: { path: "asc" },
  });
});
