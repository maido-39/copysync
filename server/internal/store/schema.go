package store

// Bucket names used in the bbolt database. The queues bucket holds one nested
// sub-bucket per device id, whose keys are big-endian uint64 sequence numbers
// giving FIFO ordering.
var (
	bucketMeta        = []byte("meta")
	bucketAdmin       = []byte("admin")
	bucketSessions    = []byte("sessions")
	bucketDevices     = []byte("devices")
	bucketDeviceNames = []byte("device_names") // lowercased name -> device id (uniqueness index)
	bucketTokens      = []byte("tokens")
	bucketTokenIndex  = []byte("token_index") // token hash -> device id (blob-channel auth)
	bucketPairing     = []byte("pairing")
	bucketSettings    = []byte("settings")
	bucketQueues      = []byte("queues")
	bucketBlobs       = []byte("blobs")
)

var allBuckets = [][]byte{
	bucketMeta, bucketAdmin, bucketSessions, bucketDevices, bucketDeviceNames,
	bucketTokens, bucketTokenIndex, bucketPairing, bucketSettings, bucketQueues, bucketBlobs,
}
