import { beforeEach, describe, expect, it } from "vitest";
import { taskStore } from "./taskStore";
import { mockTaskA, mockTaskB, mockTaskC } from "@/test/fixtures";

describe("taskStore", () => {
  beforeEach(() => {
    taskStore.getState().clearTasks();
  });

  it("addTask adds a task", () => {
    taskStore.getState().addTask(mockTaskA);

    expect(taskStore.getState().getTask(mockTaskA.id)).toEqual(mockTaskA);
  });

  it("updateTask updates an existing task", () => {
    taskStore.getState().addTask(mockTaskA);
    const updated = { ...mockTaskA, title: "Updated Task One", status: "closed" };

    taskStore.getState().updateTask(updated);

    expect(taskStore.getState().getTask(mockTaskA.id)).toEqual(updated);
  });

  it("removeTask removes a task", () => {
    taskStore.getState().addTask(mockTaskA);

    taskStore.getState().removeTask(mockTaskA.id);

    expect(taskStore.getState().getTask(mockTaskA.id)).toBeUndefined();
  });

  it("setTasks replaces all tasks", () => {
    taskStore.getState().addTask(mockTaskA);

    taskStore.getState().setTasks([mockTaskB, mockTaskC]);

    expect(taskStore.getState().getAllTasks()).toHaveLength(2);
    expect(taskStore.getState().getTask(mockTaskA.id)).toBeUndefined();
    expect(taskStore.getState().getTask(mockTaskB.id)).toEqual(mockTaskB);
  });

  it("clearTasks empties the task map", () => {
    taskStore.getState().setTasks([mockTaskA, mockTaskB]);

    taskStore.getState().clearTasks();

    expect(taskStore.getState().getAllTasks()).toEqual([]);
  });

  it("getTasksByEpic filters tasks by epic_id", () => {
    taskStore.getState().setTasks([mockTaskA, mockTaskB, mockTaskC]);

    const epicTasks = taskStore.getState().getTasksByEpic("epic-1");

    expect(epicTasks).toHaveLength(2);
    expect(epicTasks.map((t) => t.id)).toEqual([mockTaskA.id, mockTaskC.id]);
  });

  it("getTasksByStatus filters tasks by status", () => {
    taskStore.getState().setTasks([mockTaskA, mockTaskB, mockTaskC]);

    const openTasks = taskStore.getState().getTasksByStatus("open");

    expect(openTasks).toHaveLength(2);
    expect(openTasks.map((t) => t.id)).toEqual([mockTaskA.id, mockTaskC.id]);
  });

  it("addTask overwrites task with same id", () => {
    taskStore.getState().addTask(mockTaskA);
    const replacement = { ...mockTaskA, title: "Replacement" };

    taskStore.getState().addTask(replacement);

    expect(taskStore.getState().getAllTasks()).toHaveLength(1);
    expect(taskStore.getState().getTask(mockTaskA.id)?.title).toBe("Replacement");
  });
});
