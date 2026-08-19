import { createSeedClient } from "@snaplet/seed";
import fs from "node:fs";

const model = JSON.parse(fs.readFileSync(".snaplet/dataModel.json", "utf8"));
const rows = Number(process.env.ROWS ?? 5);
const seed = await createSeedClient();
await seed.$resetDatabase();

const out: Record<string, string> = {};
for (const name of Object.keys(model.models)) {
  try {
    await (seed as any)[name]((x: any) => x(rows));
    out[name] = "ok";
  } catch (e: any) {
    out[name] = String(e?.message ?? e).split("\n")[0].slice(0, 200);
  }
}
console.log("RESULT " + JSON.stringify(out));
process.exit(0);
