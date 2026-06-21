import { describe, expect, it } from "vitest";

import {
  inferRelations,
  parseDataModel,
  parseDataModelBlock,
} from "./dataModel";

describe("parseDataModel — empty / fallback", () => {
  it("returns [] for empty / whitespace input", () => {
    expect(parseDataModel("")).toEqual([]);
    expect(parseDataModel("   \n  \n")).toEqual([]);
  });

  it("returns [] when there are no recognisable field lines (pure prose)", () => {
    // Single prose line has no `name:` / `name ...` shape we can parse.
    expect(parseDataModel("This is just a paragraph, no fields.")).toEqual([]);
  });

  it("names a heading-less single entity from the fallback `name` attribute", () => {
    const entities = parseDataModel("- id: uuid\n- email: text", "User");
    expect(entities).toHaveLength(1);
    expect(entities[0]!.name).toBe("User");
    expect(entities[0]!.fields.map((f) => f.name)).toEqual(["id", "email"]);
  });

  it("falls back to 'Model' when no name and no heading is given", () => {
    const entities = parseDataModel("- id: uuid");
    expect(entities[0]!.name).toBe("Model");
  });
});

describe("parseDataModel — markdown table", () => {
  it("parses a header + separator + rows table into fields", () => {
    const body = [
      "| field    | type | notes |",
      "| -------- | ---- | ----- |",
      "| id       | uuid | PK    |",
      "| email    | text | unique email |",
      "| owner_id | uuid | FK users.id, nullable |",
    ].join("\n");
    const entities = parseDataModel(body, "Account");
    expect(entities).toHaveLength(1);
    const fields = entities[0]!.fields;
    expect(fields.map((f) => f.name)).toEqual(["id", "email", "owner_id"]);
    expect(fields[0]).toMatchObject({ type: "uuid", pk: true });
    expect(fields[2]).toMatchObject({
      type: "uuid",
      nullable: true,
      fk: { table: "users", column: "id" },
    });
  });

  it("detects the name/type columns by header label regardless of order", () => {
    const body = [
      "| Column | Notes | Type |",
      "| --- | --- | --- |",
      "| id | primary key | bigint |",
    ].join("\n");
    const fields = parseDataModel(body)[0]!.fields;
    expect(fields[0]).toMatchObject({ name: "id", type: "bigint", pk: true });
  });

  it("strips backticks from cell values", () => {
    const body = [
      "| field | type |",
      "| --- | --- |",
      "| `id` | `uuid` |",
    ].join("\n");
    const fields = parseDataModel(body)[0]!.fields;
    expect(fields[0]).toMatchObject({ name: "id", type: "uuid" });
  });
});

describe("parseDataModel — bulleted / plain list", () => {
  it("parses `- name: type (hints)` list rows", () => {
    const body = [
      "- id: uuid (PK)",
      "- owner_id: uuid — FK users.id, nullable",
      "- status: text, default = 'active'",
    ].join("\n");
    const fields = parseDataModel(body, "Item")[0]!.fields;
    expect(fields.map((f) => f.name)).toEqual(["id", "owner_id", "status"]);
    expect(fields[0]).toMatchObject({ type: "uuid", pk: true });
    expect(fields[1]).toMatchObject({
      type: "uuid",
      nullable: true,
      fk: { table: "users", column: "id" },
    });
    expect(fields[2]).toMatchObject({ type: "text", default: "active" });
  });

  it("parses plain (no-bullet) `name type` lines", () => {
    const body = "id uuid PK\nname text";
    const fields = parseDataModel(body)[0]!.fields;
    expect(fields.map((f) => f.name)).toEqual(["id", "name"]);
    expect(fields[0]!.pk).toBe(true);
    expect(fields[1]).toMatchObject({ type: "text" });
  });

  it("keeps leftover prose lines as the entity note, not a field", () => {
    const body = "Stores user accounts.\n- id: uuid (PK)";
    const entity = parseDataModel(body, "User")[0]!;
    expect(entity.fields.map((f) => f.name)).toEqual(["id"]);
    expect(entity.note).toContain("Stores user accounts.");
  });
});

describe("parseDataModel — multi-entity sections", () => {
  it("splits `### EntityName` sections into separate entities", () => {
    const body = [
      "### User",
      "- id: uuid (PK)",
      "- name: text",
      "",
      "### Order",
      "| field | type | notes |",
      "| --- | --- | --- |",
      "| id | uuid | PK |",
      "| user_id | uuid | FK User.id |",
    ].join("\n");
    const entities = parseDataModel(body);
    expect(entities.map((e) => e.name)).toEqual(["User", "Order"]);
    expect(entities[0]!.fields.map((f) => f.name)).toEqual(["id", "name"]);
    expect(entities[1]!.fields.map((f) => f.name)).toEqual(["id", "user_id"]);
    expect(entities[1]!.fields[1]!.fk).toMatchObject({ table: "User" });
  });

  it("strips a `table:` / `entity:` label and backticks from headings", () => {
    const body = ["## table: `orders`", "- id: uuid (PK)"].join("\n");
    expect(parseDataModel(body)[0]!.name).toBe("orders");
  });

  it("reads a bold-only line as an entity heading", () => {
    const body = ["**Sessions**", "- id: uuid (PK)"].join("\n");
    expect(parseDataModel(body)[0]!.name).toBe("Sessions");
  });
});

describe("hint detection — PK / FK / nullable / default / enum", () => {
  it("detects PK from `PK`, `primary key`", () => {
    expect(parseDataModel("id uuid primary key")[0]!.fields[0]!.pk).toBe(true);
    expect(parseDataModel("id uuid PK")[0]!.fields[0]!.pk).toBe(true);
  });

  it("does not fire PK on a non-boundary substring", () => {
    expect(parseDataModel("spkfield text")[0]!.fields[0]!.pk).toBeUndefined();
  });

  it("parses FK with and without a column, and `references` syntax", () => {
    expect(parseDataModel("a uuid FK users.id")[0]!.fields[0]!.fk).toEqual({
      table: "users",
      column: "id",
    });
    expect(parseDataModel("b uuid FK users")[0]!.fields[0]!.fk).toEqual({
      table: "users",
      column: undefined,
    });
    expect(
      parseDataModel("c uuid references orders(id)")[0]!.fields[0]!.fk,
    ).toEqual({ table: "orders", column: "id" });
  });

  it("detects nullable but treats `not null` as non-nullable", () => {
    expect(parseDataModel("a text nullable")[0]!.fields[0]!.nullable).toBe(true);
    expect(
      parseDataModel("b text not null")[0]!.fields[0]!.nullable,
    ).toBeUndefined();
  });

  it("extracts a default value, unwrapping quotes", () => {
    expect(parseDataModel("a text default = 'active'")[0]!.fields[0]!.default).toBe(
      "active",
    );
    expect(parseDataModel("b int default 0")[0]!.fields[0]!.default).toBe("0");
  });

  it("extracts enum values from `enum: a | b | c`", () => {
    const field = parseDataModel("status text enum: active | archived | deleted")[0]!
      .fields[0]!;
    expect(field.enumValues).toEqual(["active", "archived", "deleted"]);
  });

  it("extracts a bare pipe-separated enum list", () => {
    const field = parseDataModel("role text admin | member | guest")[0]!.fields[0]!;
    expect(field.enumValues).toEqual(["admin", "member", "guest"]);
  });
});

describe("inferRelations", () => {
  it("infers a 1-n relation from each FK field, resolving the target", () => {
    const entities = parseDataModel(
      [
        "### User",
        "- id: uuid (PK)",
        "### Order",
        "- id: uuid (PK)",
        "- user_id: uuid FK User.id",
      ].join("\n"),
    );
    const relations = inferRelations(entities);
    expect(relations).toHaveLength(1);
    expect(relations[0]).toMatchObject({
      from: "Order",
      to: "User",
      kind: "1-n",
      label: "user_id",
      unresolved: false,
    });
  });

  it("tolerates singular/plural target names", () => {
    const entities = parseDataModel(
      ["### users", "- id: uuid (PK)", "### posts", "- author: uuid FK user.id"].join(
        "\n",
      ),
    );
    const relations = inferRelations(entities);
    expect(relations[0]).toMatchObject({ to: "users", unresolved: false });
  });

  it("marks an FK whose target isn't a known entity as unresolved", () => {
    const entities = parseDataModel(
      ["### Order", "- id: uuid (PK)", "- account_id: uuid FK accounts.id"].join(
        "\n",
      ),
    );
    const relations = inferRelations(entities);
    expect(relations[0]).toMatchObject({ to: "accounts", unresolved: true });
  });

  it("produces no relations when there are no FK fields", () => {
    const entities = parseDataModel("- id: uuid (PK)\n- name: text", "User");
    expect(inferRelations(entities)).toEqual([]);
  });
});

describe("parseDataModelBlock — end to end", () => {
  it("returns entities + inferred relations together", () => {
    const body = [
      "### User",
      "| field | type | notes |",
      "| --- | --- | --- |",
      "| id | uuid | PK |",
      "| email | text | unique, nullable |",
      "",
      "### Post",
      "| field | type | notes |",
      "| --- | --- | --- |",
      "| id | uuid | PK |",
      "| author_id | uuid | FK User.id |",
      "| status | text | enum: draft | published |",
    ].join("\n");
    const { entities, relations } = parseDataModelBlock(body);
    expect(entities.map((e) => e.name)).toEqual(["User", "Post"]);
    expect(entities[1]!.fields[2]!.enumValues).toEqual(["draft", "published"]);
    expect(relations).toHaveLength(1);
    expect(relations[0]).toMatchObject({
      from: "Post",
      to: "User",
      unresolved: false,
    });
  });
});
