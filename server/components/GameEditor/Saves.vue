<template>
  <div class="px-4 sm:px-6 lg:px-8 py-8">
    <div class="sm:flex sm:items-center">
      <h1 class="sm:flex-auto text-base font-semibold text-white">
        {{ $t("library.admin.saves.title") }}
      </h1>
      <div class="mt-4 sm:mt-0 sm:ml-16 flex gap-x-2">
        <button
          type="button"
          :disabled="busy"
          class="inline-flex items-center rounded-md bg-zinc-800 px-3 py-1 text-sm font-semibold font-display text-white shadow-sm hover:bg-zinc-700 disabled:opacity-50"
          @click="suggest"
        >
          {{ $t("library.admin.saves.suggest") }}
        </button>
        <button
          type="button"
          :disabled="busy"
          class="inline-flex items-center rounded-md bg-zinc-800 px-3 py-1 text-sm font-semibold font-display text-white shadow-sm hover:bg-zinc-700 disabled:opacity-50"
          @click="add"
        >
          {{ $t("library.admin.saves.add") }}
        </button>
        <button
          type="button"
          :disabled="busy || !dirty"
          class="inline-flex items-center rounded-md bg-blue-600 px-3 py-1 text-sm font-semibold font-display text-white shadow-sm hover:bg-blue-700 disabled:bg-blue-800/50"
          @click="save"
        >
          {{ $t("library.admin.saves.save") }}
        </button>
      </div>
    </div>

    <table
      v-if="rows.length > 0"
      class="mt-8 min-w-full divide-y divide-white/15"
    >
      <thead>
        <tr>
          <th
            v-for="heading in headings"
            :key="heading"
            scope="col"
            class="py-3 pr-3 text-left text-xs font-medium tracking-wide text-gray-400 uppercase"
          >
            {{ heading }}
          </th>
          <th />
        </tr>
      </thead>
      <tbody class="divide-y divide-white/10">
        <tr v-for="(row, index) in rows" :key="index">
          <td class="py-2 pr-3 w-full">
            <input
              v-model="row.path"
              type="text"
              class="block w-full rounded-md bg-zinc-800 px-3 py-1.5 font-mono text-sm text-zinc-100 outline outline-1 -outline-offset-1 outline-zinc-700 focus:outline-2 focus:-outline-offset-2 focus:outline-blue-600"
              @input="dirty = true"
            />
          </td>
          <td class="py-2 pr-3">
            <select
              v-model="row.platform"
              class="rounded-md bg-zinc-800 py-1.5 pl-3 pr-8 text-sm text-zinc-100 outline outline-1 -outline-offset-1 outline-zinc-700"
              @change="dirty = true"
            >
              <option
                v-for="option in platformOptions"
                :key="option.value ?? 'any'"
                :value="option.value"
              >
                {{ option.label }}
              </option>
            </select>
          </td>
          <td class="py-2 pr-3 text-sm text-zinc-400 whitespace-nowrap">
            {{ sourceLabels[row.source] }}
          </td>
          <td class="py-2">
            <button
              type="button"
              class="text-zinc-400 hover:text-red-400"
              :aria-label="$t('library.admin.saves.remove')"
              @click="remove(index)"
            >
              <XMarkIcon class="size-5" />
            </button>
          </td>
        </tr>
      </tbody>
    </table>
    <p v-else class="mt-8 text-sm text-zinc-400">
      {{ $t("library.admin.saves.none") }}
    </p>
  </div>
</template>

<script setup lang="ts">
import type { SerializeObject } from "nitropack";
import { XMarkIcon } from "@heroicons/vue/24/outline";
import type { AdminFetchGameType } from "~/server/api/v1/admin/game/[id]/index.get";

defineProps<{ unimportedVersions: string[] }>();
const game = defineModel<SerializeObject<AdminFetchGameType>>({
  required: true,
});

const loaded = await $dropFetch("/api/v1/admin/game/:id/savelocations", {
  params: { id: game.value.id },
});
type Row = Pick<
  (typeof loaded.locations)[number],
  "path" | "platform" | "source"
>;

const { t } = useI18n();
const platformOptions: { value: Row["platform"]; label: string }[] = [
  { value: null, label: t("library.admin.saves.any") },
  { value: "Windows", label: t("library.admin.saves.platforms.windows") },
  { value: "macOS", label: t("library.admin.saves.platforms.macos") },
  { value: "Linux", label: t("library.admin.saves.platforms.linux") },
];
const sourceLabels: Record<Row["source"], string> = {
  Manual: t("library.admin.saves.sources.manual"),
  Ludusavi: t("library.admin.saves.sources.ludusavi"),
  Engine: t("library.admin.saves.sources.engine"),
};
const headings = [
  t("library.admin.saves.path"),
  t("library.admin.saves.platform"),
  t("library.admin.saves.source"),
];

const rows = ref<Row[]>(loaded.locations.map(strip));
const dirty = ref(false);
const busy = ref(false);

function strip(l: Row): Row {
  return { path: l.path, platform: l.platform, source: l.source };
}

function add() {
  rows.value.push({ path: "", platform: null, source: "Manual" });
  dirty.value = true;
}

function remove(index: number) {
  rows.value.splice(index, 1);
  dirty.value = true;
}

async function suggest() {
  busy.value = true;
  try {
    const suggested = await $dropFetch(
      "/api/v1/admin/game/:id/savelocations/suggest",
      {
        method: "POST",
        params: { id: game.value.id },
        failTitle: t("library.admin.saves.suggestFailed"),
      },
    );
    const have = new Set(rows.value.map((r) => `${r.platform}|${r.path}`));
    const fresh = suggested.filter((s) => !have.has(`${s.platform}|${s.path}`));
    rows.value.push(...fresh.map(strip));
    if (fresh.length > 0) dirty.value = true;
  } finally {
    busy.value = false;
  }
}

async function save() {
  busy.value = true;
  try {
    const saved = await $dropFetch(
      "/api/v1/admin/game/:id/savelocations",
      {
        method: "PUT",
        params: { id: game.value.id },
        body: { locations: rows.value.filter((r) => r.path.trim()) },
        failTitle: t("library.admin.saves.saveFailed"),
      },
    );
    rows.value = saved.map(strip);
    dirty.value = false;
  } finally {
    busy.value = false;
  }
}
</script>
