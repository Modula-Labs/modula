// The app's icon set: Tabler (Kobra's icons) at Kobra's 1.8 stroke. Import icons
// from here, not from @tabler/icons-react, so stroke and size stay consistent.
import * as T from "@tabler/icons-react";
import type { ComponentType } from "react";

type IconProps = T.IconProps;

function kobra(Icon: ComponentType<IconProps>) {
  const Wrapped = (props: IconProps) => <Icon stroke={1.8} {...props} />;
  Wrapped.displayName = Icon.displayName;
  return Wrapped;
}

export const Activity = kobra(T.IconActivity);
export const ArrowLeft = kobra(T.IconArrowLeft);
export const Box = kobra(T.IconBox);
export const BrandDocker = kobra(T.IconBrandDocker);
export const Check = kobra(T.IconCheck);
export const ChevronDown = kobra(T.IconChevronDown);
export const ChevronRight = kobra(T.IconChevronRight);
export const Circle = kobra(T.IconCircle);
export const Copy = kobra(T.IconCopy);
export const Database = kobra(T.IconDatabase);
export const DotsVertical = kobra(T.IconDotsVertical);
export const Download = kobra(T.IconDownload);
export const Eye = kobra(T.IconEye);
export const EyeOff = kobra(T.IconEyeOff);
export const FileText = kobra(T.IconFileText);
export const Key = kobra(T.IconKey);
export const LayoutGrid = kobra(T.IconLayoutGrid);
export const List = kobra(T.IconList);
export const Lock = kobra(T.IconLock);
export const Logout = kobra(T.IconLogout);
export const Maximize = kobra(T.IconMaximize);
export const Minus = kobra(T.IconMinus);
export const Moon = kobra(T.IconMoon);
export const Phone = kobra(T.IconDeviceMobile);
export const Plus = kobra(T.IconPlus);
export const Refresh = kobra(T.IconRefresh);
export const Rocket = kobra(T.IconRocket);
export const Search = kobra(T.IconSearch);
export const Server = kobra(T.IconServer);
export const Settings = kobra(T.IconSettings);
export const ShieldLock = kobra(T.IconShieldLock);
export const Sun = kobra(T.IconSun);
export const Terminal = kobra(T.IconTerminal2);
export const Trash = kobra(T.IconTrash);
export const Variable = kobra(T.IconVariable);
export const World = kobra(T.IconWorld);
export const X = kobra(T.IconX);
export const Zap = kobra(T.IconBolt);
