import {
  createStore,
  type Mutate,
  type StateCreator,
  type StoreApi,
} from "zustand/vanilla";
import { subscribeWithSelector } from "zustand/middleware";
import { immer } from "zustand/middleware/immer";

export type Store<T> = Mutate<
  StoreApi<T>,
  [["zustand/subscribeWithSelector", never], ["zustand/immer", never]]
>;

export function create<T>(initializer: () => T): Store<T> {
  const stateCreator: StateCreator<
    T,
    [["zustand/immer", never]],
    [],
    T
  > = () => initializer();
  return createStore<T>()(subscribeWithSelector(immer(stateCreator)));
}
