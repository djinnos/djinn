/**
 * Task and Epic types for DjinnOS Desktop
 * 
 * These types mirror the server-side types for full-entity SSE events.
 */

export type TaskStatus = 'pending' | 'in_progress' | 'completed' | 'blocked';
export type TaskPriority = 'P0' | 'P1' | 'P2' | 'P3';

export interface Task {
  id: string;
  shortId?: string;
  title: string;
  description: string;
  design: string;
  acceptanceCriteria: string[];
  activity: string[];
  status: TaskStatus;
  reviewPhase?: 'needs_task_review' | 'in_task_review';
  priority: TaskPriority;
  epicId: string | null;
  labels: string[];
  owner: string | null;
  createdAt: string;
  updatedAt: string;
  sessionCount?: number;
  trackedSeconds?: number;
  activeSessionStartedAt?: string | null;
}

export type EpicStatus = 'active' | 'completed' | 'archived';

export interface Epic {
  id: string;
  title: string;
  description: string;
  status: EpicStatus;
  priority: TaskPriority;
  labels: string[];
  owner: string | null;
  createdAt: string;
  updatedAt: string;
  sessionCount?: number;
  trackedSeconds?: number;
  activeSessionStartedAt?: string | null;
}

// SSE Event payloads
export interface TaskCreatedPayload extends Task {}
export interface TaskUpdatedPayload extends Task {}
export interface TaskDeletedPayload {
  id: string;
}

export interface EpicCreatedPayload extends Epic {}
export interface EpicUpdatedPayload extends Epic {}
export interface EpicDeletedPayload {
  id: string;
}


export interface Project {
  id: string;
  name: string;
  path?: string;
  description?: string;
}
