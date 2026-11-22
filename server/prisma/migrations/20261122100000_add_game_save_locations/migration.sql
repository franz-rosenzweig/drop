-- CreateEnum
CREATE TYPE "SaveLocationSource" AS ENUM ('manual', 'ludusavi', 'engine');

-- AlterTable
ALTER TABLE "Game" ADD COLUMN "saveLocationsLocked" BOOLEAN NOT NULL DEFAULT false;

-- CreateTable
CREATE TABLE "GameSaveLocation" (
    "id" TEXT NOT NULL,
    "gameId" TEXT NOT NULL,
    "path" TEXT NOT NULL,
    "platform" "Platform",
    "source" "SaveLocationSource" NOT NULL DEFAULT 'manual',

    CONSTRAINT "GameSaveLocation_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE INDEX "GameSaveLocation_gameId_idx" ON "GameSaveLocation"("gameId");

-- AddForeignKey
ALTER TABLE "GameSaveLocation" ADD CONSTRAINT "GameSaveLocation_gameId_fkey" FOREIGN KEY ("gameId") REFERENCES "Game"("id") ON DELETE CASCADE ON UPDATE CASCADE;
