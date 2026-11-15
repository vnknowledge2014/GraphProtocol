import { generateTypesFromSdl } from "./codegen.ts";

export const WALKING_SKELETON_SDL = `
enum CustomerTier {
  standard
  vip
}

enum OrderStatus {
  pending
  paid
  shipped
}

type Customer {
  id: Int!
  name: String!
  tier: CustomerTier!
}

type Order {
  id: Int!
  customer_id: Int!
  total_cents: Int!
  status: OrderStatus!
}

type CustomerNote {
  note_id: String!
  customer_id: Int!
  summary: String!
}

type CustomerNotesPayload {
  customer_id: Int!
  notes: [CustomerNote!]!
}

type Query {
  customers(limit: Int, offset: Int): [Customer!]!
  customer(id: Int!): Customer
  orders(customer_id: Int): [Order!]!
  order(id: Int!): Order
  customer_notes(customer_id: Int!): CustomerNotesPayload
}
`;

// Typed TypeScript declarations derived via generateTypesFromSdl(WALKING_SKELETON_SDL).
export type CustomerTier = "standard" | "vip";
export type OrderStatus = "pending" | "paid" | "shipped";

export interface Customer {
  id: number;
  name: string;
  tier: CustomerTier;
}

export interface Order {
  id: number;
  customer_id: number;
  total_cents: number;
  status: OrderStatus;
}

export interface CustomerNote {
  note_id: string;
  customer_id: number;
  summary: string;
}

export interface CustomerNotesPayload {
  customer_id: number;
  notes: Array<CustomerNote>;
}

export interface WalkingSkeletonQuery {
  customers?: Array<Customer>;
  customer?: Customer | null;
  orders?: Array<Order>;
  order?: Order | null;
  customer_notes?: CustomerNotesPayload | null;
}

export function renderWalkingSkeletonTypes(): string {
  return generateTypesFromSdl(WALKING_SKELETON_SDL, "examples/walking-skeleton");
}
