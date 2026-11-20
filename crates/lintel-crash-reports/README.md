# Lintel crash reports

`lintel-crash-reports` provides Lintel's local minidump queue and the `lintel-crash-uploader` executable. Applications configure the service through the `lintel` facade and package the executable using `lintel package --crash-uploader`.

The queue stores reports, upload policy and receiver receipts in an application-owned directory. The application passes the receiver settings to the uploader over its inherited input pipe. Changing a bearer token requires advancing its credential generation. Receiver responses must identify the uploaded report and include a durable receipt before the queue records delivery.

The queue reloads the saved policy for the same receiver on each launch. Choosing a new receiver starts with the application’s startup policy and keeps earlier delivery states scoped to the original receiver. An explicit `Disabled` startup policy revokes a saved grant.
