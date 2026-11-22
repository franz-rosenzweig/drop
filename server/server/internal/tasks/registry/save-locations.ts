import prisma from "~/server/internal/db/database";
import { defineDropTask } from "..";
import { suggestSaveLocations } from "../../saves/locations";

// Fills save locations for every game that has none and whose list an admin
// hasn't edited. Runs weekly, so new games pick theirs up on their own.
export default defineDropTask({
  buildId: () => `saves:fill-locations:${new Date().toISOString()}`,
  name: "Fill save locations",
  acls: ["system:game:read"],
  taskGroup: "saves:fill-locations",
  async run({ progress, logger }) {
    const games = await prisma.game.findMany({
      where: { saveLocationsLocked: false, saveLocations: { none: {} } },
      select: { id: true, mName: true },
    });
    logger.info(`Looking up save locations for ${games.length} games`);

    let filled = 0;
    for (const [i, game] of games.entries()) {
      const locations = await suggestSaveLocations(game.id);
      if (locations.length > 0) {
        await prisma.gameSaveLocation.createMany({
          data: locations.map((l) => ({ ...l, gameId: game.id })),
        });
        filled++;
        logger.info(`${game.mName}: ${locations.length} save locations`);
      }
      progress(((i + 1) / games.length) * 100);
    }

    logger.info(`Filled save locations for ${filled} of ${games.length} games`);
    progress(100);
  },
});
