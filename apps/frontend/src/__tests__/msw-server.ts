import { setupServer } from "msw/node";
import { handlers } from "./handlers";

// Single MSW server instance shared across all Vitest tests.
export const server = setupServer(...handlers);
