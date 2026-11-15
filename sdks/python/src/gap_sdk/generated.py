"""Generated walking-skeleton Python types derived from WALKING_SKELETON_SDL."""

from __future__ import annotations

from typing import Literal, TypedDict

from .codegen import generate_types_from_sdl

WALKING_SKELETON_SDL = """
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
"""

CustomerTier = Literal["standard", "vip"]
OrderStatus = Literal["pending", "paid", "shipped"]


class Customer(TypedDict):
    id: int
    name: str
    tier: CustomerTier


class Order(TypedDict):
    id: int
    customer_id: int
    total_cents: int
    status: OrderStatus


class CustomerNote(TypedDict):
    note_id: str
    customer_id: int
    summary: str


class CustomerNotesPayload(TypedDict):
    customer_id: int
    notes: list[CustomerNote]


class WalkingSkeletonQuery(TypedDict, total=False):
    customers: list[Customer]
    customer: Customer | None
    orders: list[Order]
    order: Order | None
    customer_notes: CustomerNotesPayload | None


def render_walking_skeleton_types() -> str:
    return generate_types_from_sdl(WALKING_SKELETON_SDL, "examples/walking-skeleton")
