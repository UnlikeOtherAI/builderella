// Queries and mutations, one per API operation the views use.

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useState, useSyncExternalStore } from 'react';
import { api, getToken, subscribeToken, type ListJobsQuery } from './client';
import { usePollInterval } from './live';
import type {
  CreatePairingCodeRequest,
  CreateTokenRequest,
  ProjectPolicy,
  UpdateMachineRequest,
} from './schemas';
import { serverNow, setServerTime } from '../lib/clock';

export const useToken = () => useSyncExternalStore(subscribeToken, getToken, () => null);

/** The controller's current time, re-read every `intervalMs`. */
export function useNow(intervalMs = 1000): number {
  const [now, setNow] = useState(serverNow);
  useEffect(() => {
    const timer = setInterval(() => setNow(serverNow()), intervalMs);
    return () => clearInterval(timer);
  }, [intervalMs]);
  return now;
}

export function useWhoAmI() {
  return useQuery({
    queryKey: ['whoami'],
    queryFn: ({ signal }) => api.whoami(signal),
    staleTime: 60_000,
  });
}

export function useFleet() {
  const refetchInterval = usePollInterval();
  return useQuery({
    queryKey: ['fleet'],
    queryFn: async ({ signal }) => {
      const fleet = await api.fleet(signal);
      setServerTime(fleet.server_time);
      return fleet;
    },
    refetchInterval,
  });
}

export function useQueue() {
  const refetchInterval = usePollInterval();
  return useQuery({ queryKey: ['queue'], queryFn: ({ signal }) => api.queue(signal), refetchInterval });
}

export function useJobs(query: ListJobsQuery) {
  const refetchInterval = usePollInterval();
  return useQuery({
    queryKey: ['jobs', query],
    queryFn: ({ signal }) => api.jobs(query, signal),
    refetchInterval,
  });
}

export function useJob(id: string | undefined) {
  const refetchInterval = usePollInterval();
  return useQuery({
    queryKey: ['job', id],
    queryFn: ({ signal }) => api.job(id as string, signal),
    enabled: id !== undefined,
    refetchInterval,
  });
}

/**
 * A job's frozen specification. Lists do not carry it, and it never changes
 * once the job exists, so it is fetched once per job and kept.
 */
export function useJobSpec(id: string, enabled = true) {
  return useQuery({
    queryKey: ['job-spec', id],
    queryFn: async ({ signal }) => (await api.job(id, signal)).spec ?? null,
    enabled,
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: 30 * 60_000,
  });
}

export function useArtifacts(id: string | undefined) {
  const refetchInterval = usePollInterval();
  return useQuery({
    queryKey: ['artifacts', id],
    queryFn: ({ signal }) => api.artifacts(id as string, signal),
    enabled: id !== undefined,
    refetchInterval,
  });
}

export function useEnvironments() {
  const refetchInterval = usePollInterval();
  return useQuery({
    queryKey: ['environments'],
    queryFn: ({ signal }) => api.environments(signal),
    refetchInterval,
  });
}

export function useEnvironment(id: string | undefined) {
  const refetchInterval = usePollInterval();
  return useQuery({
    queryKey: ['environment', id],
    queryFn: ({ signal }) => api.environment(id as string, signal),
    enabled: id !== undefined,
    refetchInterval,
  });
}

export function useProjects() {
  const refetchInterval = usePollInterval();
  return useQuery({
    queryKey: ['projects'],
    queryFn: ({ signal }) => api.projects(signal),
    refetchInterval,
  });
}

// --------------------------------------------------------------- mutations

export function useCancelJob(id: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => api.cancelJob(id),
    onSuccess: (job) => {
      queryClient.setQueryData(['job', id], (previous: typeof job | undefined) => ({
        ...job,
        spec: job.spec ?? previous?.spec,
      }));
      void queryClient.invalidateQueries({ queryKey: ['queue'] });
      void queryClient.invalidateQueries({ queryKey: ['jobs'] });
      void queryClient.invalidateQueries({ queryKey: ['fleet'] });
    },
  });
}

export function useEnvironmentAction(id: string, action: 'renew' | 'finish') {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => (action === 'renew' ? api.renewEnvironment(id) : api.finishEnvironment(id)),
    onSuccess: (environment) => {
      queryClient.setQueryData(['environment', id], environment);
      void queryClient.invalidateQueries({ queryKey: ['environments'] });
      void queryClient.invalidateQueries({ queryKey: ['fleet'] });
    },
  });
}

export function useUpdateMachine(id: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (body: UpdateMachineRequest) => api.updateMachine(id, body),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['fleet'] }),
  });
}

export function useCreatePairingCode() {
  return useMutation({ mutationFn: (body: CreatePairingCodeRequest) => api.createPairingCode(body) });
}

export function usePutProject(name: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (policy: ProjectPolicy) => api.putProject(name, policy),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['projects'] });
      void queryClient.invalidateQueries({ queryKey: ['queue'] });
    },
  });
}

export function useCreateToken() {
  return useMutation({ mutationFn: (body: CreateTokenRequest) => api.createToken(body) });
}
