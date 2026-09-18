-- One armed retry per task after a root turn exhausts its iteration budget.
CREATE TABLE execution_escalations (
    task TEXT PRIMARY KEY
);
