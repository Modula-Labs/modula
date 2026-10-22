import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { api, type Me, type Membership } from "./api";
import { load, save } from "./storage";

export function useMe() {
  return useQuery({ queryKey: ["me"], queryFn: api.me, staleTime: 30_000 });
}

/** The selected workspace: remembered per browser, validated against memberships. */
export function useCurrentWorkspace(me: Me | undefined): [Membership | undefined, (id: string) => void] {
  const [selected, setSelected] = useState(() => load("mc.workspace"));
  const current = me?.workspaces.find((w) => w.id === selected) ?? me?.workspaces[0];
  return [
    current,
    (id: string) => {
      save("mc.workspace", id);
      setSelected(id);
    },
  ];
}
