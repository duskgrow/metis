const approvalMarker = "METIS_TEST_APPROVAL";
const cancelMarker = "METIS_TEST_APPROVAL_CANCEL";

export const inject = ["approval"];

export function apply(ctx) {
  console.log("[metis-test] approval fixture loaded");
  ctx.on(
    "agent/created",
    ({ agent }) => {
      console.log("[metis-test] agent attached");
      agent.ctx.on("session/event", (session, event) => {
        console.log(`[metis-test] session event ${event.type}`);
        if (
          event.type !== "user/message"
          || !Array.isArray(event.data.content)
        ) {
          return;
        }
        const text = event.data.content
          .filter((part) => part.type === "text")
          .map((part) => part.text)
          .join("\n");
        if (!text.includes(approvalMarker)) return;
        console.log("[metis-test] approval marker matched");
        console.log("[metis-test] approval requested");
        return Promise.resolve()
          .then(() =>
            ctx.approval.request({
              agent,
              toolName: "bash",
              reason: "Metis integration test approval",
            })
          )
          .then((outcome) => {
            console.log("[metis-test] approval resolved");
            if (text.includes(cancelMarker)) {
              agent.cancel({ kind: "user" });
            }
            return outcome;
          })
          .catch((error) => {
            console.log(
              `[metis-test] approval failed: ${String(error)}`,
            );
            throw error;
          });
      });
    },
    { global: true },
  );
}
