import { useEffect, useState } from "react";

/** Whether the window itself is glass: on macOS always, elsewhere only once the shell says the system applied its material. */
export function useWindowGlass(mac: boolean, ask: () => Promise<boolean>): boolean {
  const [applied, setApplied] = useState(false);
  useEffect(() => {
    if (mac) return;
    let active = true;
    ask().then(
      (yes) => active && setApplied(yes === true),
      () => {},
    );
    return () => {
      active = false;
    };
  }, [mac]);
  return mac || applied;
}
