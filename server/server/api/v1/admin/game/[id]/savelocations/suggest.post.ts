import aclManager from "~/server/internal/acls";
import { suggestSaveLocations } from "~/server/internal/saves/locations";

// Save locations from Ludusavi and the game's engine, for an admin to review.
// Nothing is stored.
export default defineEventHandler(async (h3) => {
  const allowed = await aclManager.allowSystemACL(h3, ["game:read"]);
  if (!allowed) throw createError({ statusCode: 403 });

  const id = getRouterParam(h3, "id")!;
  try {
    return await suggestSaveLocations(id);
  } catch (e) {
    throw createError({
      statusCode: 502,
      statusMessage: `Couldn't load the Ludusavi manifest: ${e}`,
    });
  }
});
