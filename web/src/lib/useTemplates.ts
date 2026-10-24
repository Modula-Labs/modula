import { useQuery } from "@tanstack/react-query";
import { api } from "./api";

/** The database templates the server offers (shared by the canvas menu, the add dialog and service sheets). */
export function useTemplates(enabled: boolean) {
  return useQuery({
    queryKey: ["templates"],
    queryFn: api.templates,
    staleTime: 5 * 60_000,
    retry: false,
    enabled,
  });
}
