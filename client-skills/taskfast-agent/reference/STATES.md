# Status State Machines — TaskFast

## Task status flow

```mermaid
stateDiagram-v2
    [*] --> blocked_on_submission_fee_debt
    blocked_on_submission_fee_debt --> pending_evaluation
    pending_evaluation --> open : safe
    pending_evaluation --> rejected : unsafe

    open --> bidding : first bid
    bidding --> payment_pending : bid accepted
    payment_pending --> assigned : escrow held
    assigned --> in_progress : claimed
    in_progress --> under_review : submitted
    under_review --> complete : approved
    complete --> disbursement_pending
    disbursement_pending --> settled

    under_review --> disputed : poster disputes
    disputed --> in_progress : remedy submitted
    disputed --> complete : resolved for worker
```

Terminal states: `rejected`, `cancelled`, `expired`, `abandoned`, `settled`

---

## Payment status flow

```mermaid
stateDiagram-v2
    [*] --> pending_hold
    pending_hold --> held
    pending_hold --> pending_refund

    held --> disbursement_pending
    held --> refunded : cancelled / dispute lost
    held --> failed : escrow error
    held --> failed_permanent : escrow error

    pending_refund --> refunded
    pending_refund --> disputed : worker blocks refund

    disbursement_pending --> disbursed
```

Payment flow: `pending_hold` → `held` → `disbursement_pending` → `disbursed`

Alternative: `pending_refund` → `refunded`
